//! Durable replication of opaque push pseudonyms to recipient DHT homes.

use std::sync::Arc;

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

use super::Dht;
use super::config::K;

/// Fan one user-authorized registration to every current home. Client
/// reconnect repeats this idempotent record; no platform token is present.
/// Persist until a target home explicitly accepts it; bootstrap and transient
/// peer failures must not drop offline wake registration.
pub(crate) async fn replicate_to_homes(dht: Arc<Dht>, publish: PushPseudonymPublish) {
    if dht.store.put_pending_push(&publish).is_err() {
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
            let Ok(conn) = super::lookup::connect_to_peer(&dht, &home).await else { return false };
            let Ok(bytes) = DhtPacket::Request(DhtRequest::PushPseudonymPublish(publish)).pack()
            else {
                return false;
            };
            let Ok((mut tx, mut rx)) = conn.open_bi().await else { return false };
            if tx.write_all(&bytes).await.is_err() || tx.finish().is_err() {
                return false;
            }
            matches!(
                DhtPacket::unpack(&mut rx).await,
                Ok(DhtPacket::Response(DhtResponse::PushPseudonymPublish(
                    PushPseudonymPublishResp { accepted: true }
                )))
            )
        });
    }
    let mut accepted = self_is_home;
    while let Some(result) = set.join_next().await {
        accepted |= result.ok() == Some(true);
    }
    if accepted {
        let _ = dht.store.remove_pending_push(&publish.user_ipk.0);
    }
}

pub(crate) async fn retry_pending(dht: Arc<Dht>) {
    for publish in dht.store.pending_pushes() {
        replicate_to_homes(dht.clone(), publish).await;
    }
}

/// Validate owner signature, require target-home ownership, then fsync opaque
/// pseudonym. Gateway alone resolves it to a platform token.
pub(crate) fn handle_publish(
    dht: &Dht, publish: PushPseudonymPublish, _now_ms: u64,
) -> PushPseudonymPublishResp {
    if !valid_publish_signature(&publish)
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
