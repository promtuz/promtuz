//! Durable replication of opaque push pseudonyms to recipient DHT homes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
use common::proto::dht_p2p::PushPseudonymPublish;
use common::proto::dht_p2p::PushPseudonymPublishResp;
use common::proto::dht_p2p::push_pseudonym_signing_input;
use common::quic::id::NodeId;

use super::Dht;
use super::config::FORWARD_TIMEOUT_MS;
use super::rpc::fan_out;

/// Distinct users whose registration may be awaiting replication.
const MAX_PENDING_PUSHES: usize = 4096;

/// Pending records replayed per sweep. `Dht::push_retry_cursor` rotates the window so a
/// permanently-failing head cannot starve the tail.
const MAX_PENDING_RETRIES_PER_SWEEP: usize = 32;

/// A record no home accepts stays pending and [`retry_pending`] replays it until its signed
/// timestamp leaves [`MAX_DHT_HELLO_SKEW_MS`]; the client's next reconnect signs a fresh one.
pub(crate) async fn replicate_to_homes(dht: Arc<Dht>, publish: PushPseudonymPublish) {
    if !persist_pending(&dht, &publish) {
        return;
    }
    let (homes, self_is_home) =
        super::routing::homes(&dht, &NodeId::from_bytes(publish.user_ipk.0));
    if self_is_home {
        let _ = dht.store.put_push_pseudonym(&publish.user_ipk.0, &publish.pseudonym.0);
    }
    let req = DhtRequest::PushPseudonymPublish(publish.clone());
    let replies = fan_out(&dht, &homes, &req, FORWARD_TIMEOUT_MS).await;
    let accepted = self_is_home
        || replies.iter().any(|(_, reply)| {
            matches!(
                reply,
                DhtResponse::PushPseudonymPublish(PushPseudonymPublishResp { accepted: true })
            )
        });
    if accepted {
        let _ = dht.store.remove_pending_push(&publish.user_ipk.0);
    }
}

fn persist_pending(dht: &Dht, publish: &PushPseudonymPublish) -> bool {
    let is_new = dht.store.push_pending.get(publish.user_ipk.0).ok().flatten().is_none();
    if is_new
        && dht.store.push_pending.iter().take(MAX_PENDING_PUSHES).count() >= MAX_PENDING_PUSHES
    {
        return false;
    }
    dht.store.put_pending_push(publish).is_ok()
}

/// Spawns the replay: the scheduler calls this from a `select!` arm that must stay responsive to
/// its cancel token.
pub(crate) async fn retry_pending(dht: Arc<Dht>) {
    if dht.push_retry_in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        retry_pending_sweep(dht.clone()).await;
        dht.push_retry_in_flight.store(false, Ordering::Release);
    });
}

async fn retry_pending_sweep(dht: Arc<Dht>) {
    let pending = dht.store.pending_pushes();
    if pending.is_empty() {
        return;
    }
    let now_ms = common::utils::now_ms();
    let start = dht.push_retry_cursor.fetch_add(MAX_PENDING_RETRIES_PER_SWEEP, Ordering::Relaxed);
    for i in 0..pending.len().min(MAX_PENDING_RETRIES_PER_SWEEP) {
        let publish = &pending[start.wrapping_add(i) % pending.len()];
        if now_ms.abs_diff(publish.timestamp) > MAX_DHT_HELLO_SKEW_MS {
            let _ = dht.store.remove_pending_push(&publish.user_ipk.0);
            continue;
        }
        replicate_to_homes(dht.clone(), publish.clone()).await;
    }
}

/// The pseudonym is opaque: only the gateway resolves it to a platform token.
pub(crate) fn handle_publish(
    dht: &Dht, publish: PushPseudonymPublish, now_ms: u64,
) -> PushPseudonymPublishResp {
    if !valid_publish(&publish, now_ms)
        || !super::routing::homes(dht, &NodeId::from_bytes(publish.user_ipk.0)).1 {
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
    let msg =
        push_pseudonym_signing_input(&publish.user_ipk.0, &publish.pseudonym.0, publish.timestamp);
    common::crypto::verify_ed25519(&publish.user_ipk.0, &msg, &publish.user_sig.0).is_ok()
}
