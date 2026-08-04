//! Durable replication of opaque push pseudonyms to recipient DHT homes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
use common::proto::dht_p2p::PushPseudonymPublish;
use common::proto::dht_p2p::PushPseudonymPublishResp;
use common::proto::dht_p2p::push_pseudonym_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use tokio::time::timeout;

use super::Dht;
use super::config::FORWARD_TIMEOUT_MS;
use super::config::K;
use super::config::QUEUE_FETCH_TIMEOUT_MS;

/// Distinct users whose registration may be awaiting replication.
const MAX_PENDING_PUSHES: usize = 4096;

/// Pending records replayed per sweep. [`RETRY_CURSOR`] rotates the window so a
/// permanently-failing head cannot starve the tail.
const MAX_PENDING_RETRIES_PER_SWEEP: usize = 32;

static RETRY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static RETRY_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Fan one user-authorized registration to every current home. Client
/// reconnect repeats this idempotent record; no platform token is present.
/// A record that no home accepted is persisted and replayed by
/// [`retry_pending`] for as long as its signed timestamp stays inside
/// [`MAX_DHT_HELLO_SKEW_MS`] — past that no home will take it, and the client's
/// next reconnect supplies a freshly-signed one.
pub(crate) async fn replicate_to_homes(dht: Arc<Dht>, publish: PushPseudonymPublish) {
    if !persist_pending(&dht, &publish) {
        return;
    }
    let target = NodeId::from_bytes(publish.user_ipk.0);
    let self_is_home = super::routing::self_in_top_k(&dht, &target);
    if self_is_home {
        let _ = dht.store.put_push_pseudonym(&publish.user_ipk.0, &publish.pseudonym.0);
    }
    let mut homes = dht.routing.read().find_closest(&target, K);
    // `find_closest` excludes self. Replace farthest remote home with self.
    if self_is_home && homes.len() == K {
        homes.pop();
    }
    let mut set = tokio::task::JoinSet::new();
    for home in homes {
        let dht = dht.clone();
        let publish = publish.clone();
        set.spawn(async move {
            timeout(Duration::from_millis(FORWARD_TIMEOUT_MS), publish_one(dht, home, publish))
                .await
                .unwrap_or(false)
        });
    }

    let mut accepted = self_is_home;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(QUEUE_FETCH_TIMEOUT_MS);
    while !set.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            set.abort_all();
            break;
        }
        match timeout(remaining, set.join_next()).await {
            Ok(Some(result)) => accepted |= result.unwrap_or(false),
            Ok(None) => break,
            Err(_) => {
                set.abort_all();
                break;
            }
        }
    }
    if accepted {
        let _ = dht.store.remove_pending_push(&publish.user_ipk.0);
    }
}

async fn publish_one(
    dht: Arc<Dht>, home: common::proto::dht_p2p::NodeDescriptor, publish: PushPseudonymPublish,
) -> bool {
    let Ok(conn) = super::lookup::connect_to_peer(&dht, &home).await else { return false };
    let Ok(bytes) = DhtPacket::Request(DhtRequest::PushPseudonymPublish(publish)).pack() else {
        return false;
    };
    let Ok((mut tx, mut rx)) = conn.open_bi().await else { return false };
    if tx.write_all(&bytes).await.is_err() || tx.finish().is_err() {
        return false;
    }
    matches!(
        DhtPacket::unpack(&mut rx).await,
        Ok(DhtPacket::Response(DhtResponse::PushPseudonymPublish(PushPseudonymPublishResp {
            accepted: true
        })))
    )
}

/// Store a registration awaiting replication, refusing a new user once the
/// keyspace is at [`MAX_PENDING_PUSHES`]. Returns whether the caller should
/// proceed with the fan-out.
fn persist_pending(dht: &Dht, publish: &PushPseudonymPublish) -> bool {
    let is_new = dht.store.push_pending.get(publish.user_ipk.0).ok().flatten().is_none();
    if is_new
        && dht.store.push_pending.iter().take(MAX_PENDING_PUSHES).count() >= MAX_PENDING_PUSHES
    {
        return false;
    }
    dht.store.put_pending_push(publish).is_ok()
}

/// Replay a rotating window of pending registrations on its own task. The DHT
/// scheduler calls this from a `select!` arm and must stay responsive to its
/// cancel token, so nothing here is awaited inline.
pub(crate) async fn retry_pending(dht: Arc<Dht>) {
    if RETRY_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        retry_pending_sweep(dht).await;
        RETRY_IN_FLIGHT.store(false, Ordering::Release);
    });
}

async fn retry_pending_sweep(dht: Arc<Dht>) {
    let pending = dht.store.pending_pushes();
    if pending.is_empty() {
        return;
    }
    let now_ms = crate::util::systime().as_millis() as u64;
    let start = RETRY_CURSOR.fetch_add(MAX_PENDING_RETRIES_PER_SWEEP, Ordering::Relaxed);
    for i in 0..pending.len().min(MAX_PENDING_RETRIES_PER_SWEEP) {
        let publish = &pending[start.wrapping_add(i) % pending.len()];
        if now_ms.abs_diff(publish.timestamp) > MAX_DHT_HELLO_SKEW_MS {
            let _ = dht.store.remove_pending_push(&publish.user_ipk.0);
            continue;
        }
        replicate_to_homes(dht.clone(), publish.clone()).await;
    }
}

/// Validate owner signature and freshness, require target-home ownership, then
/// fsync opaque pseudonym. Gateway alone resolves it to a platform token.
pub(crate) fn handle_publish(
    dht: &Dht, publish: PushPseudonymPublish, now_ms: u64,
) -> PushPseudonymPublishResp {
    if !valid_publish(&publish, now_ms)
        || !super::routing::self_in_top_k(dht, &NodeId::from_bytes(publish.user_ipk.0)) {
        return PushPseudonymPublishResp { accepted: false };
    }
    PushPseudonymPublishResp {
        accepted: dht.store.put_push_pseudonym(&publish.user_ipk.0, &publish.pseudonym.0).is_ok(),
    }
}

pub(crate) fn valid_publish(publish: &PushPseudonymPublish, now_ms: u64) -> bool {
    if now_ms.abs_diff(publish.timestamp) > MAX_DHT_HELLO_SKEW_MS {
        return false;
    }
    valid_publish_signature(publish)
}

fn valid_publish_signature(publish: &PushPseudonymPublish) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(&publish.user_ipk.0) else {
        return false;
    };
    let sig = Signature::from_bytes(&publish.user_sig.0);
    if key
        .verify_strict(
            &push_pseudonym_signing_input(
                &publish.user_ipk.0,
                &publish.pseudonym.0,
                publish.timestamp,
            ),
            &sig,
        )
        .is_err()
    {
        return false;
    }
    true
}
