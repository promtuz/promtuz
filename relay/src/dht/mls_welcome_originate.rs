//! Tier-2 OUTBOUND fan-out for Welcome publish / fetch / ack.
//! The relay-side analogue of `forward.rs::forward_to_homes`,
//! specialised to the MLS Welcome queue RPCs.
//!
//! A phone delegates a Welcome operation to its home relay over the
//! `client/0` wrappers; the home originates the real `peer/1` fan-out to
//! the recipient's (publish) or the user's own (fetch/ack) K-closest
//! homes, self-storing if it is itself a home.
//!
//! - **publish** carries its user authorization *inside*
//!   `envelope.sender_sig` (forwarded intact); quorum on `Stored`.
//! - **fetch / ack** carry the phone's inner user signature over
//!   [`welcome_fetch_signing_input`](common::proto::mls_wire::welcome_fetch_signing_input)
//!   / ack, bound to *this relay's* NodeId as `requester_relay_id` —
//!   the phone learns that NodeId from the client/0 handshake. Fetch
//!   merges across homes and dedupes by `(group_id, kp_ref_used)`
//!   because each replica mints its own per-row `welcome_id`.

use std::collections::HashSet;
use std::sync::Arc;

use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::mls_wire::WelcomeAckReq;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomeFetchOutcome;
use common::proto::mls_wire::WelcomeFetchReq;
use common::proto::mls_wire::WelcomePublishOutcome;
use common::proto::mls_wire::WelcomePublishReq;
use common::quic::id::NodeId;

use super::Dht;
use super::config::FORWARD_K_MIN;
use super::mls_fanout::closest_homes_with_self;
use super::mls_fanout::fan_out_collect;
use super::mls_welcome::handle_welcome_ack;
use super::mls_welcome::handle_welcome_fetch;
use super::mls_welcome::handle_welcome_publish;
use super::mls_welcome::stash_prefix;

/// Originate a Welcome publish to the recipient's K-closest homes.
/// Authorization rides inside `envelope.sender_sig`; `timestamp` is the
/// phone's (bound into the per-publish skew check at each home). Returns
/// whether ≥ [`FORWARD_K_MIN`] homes stored it.
pub(crate) async fn originate_welcome_publish(
    dht: &Arc<Dht>, envelope: WelcomeEnvelopeP, timestamp: u64, now_ms: u64,
) -> bool {
    let target = NodeId::from_bytes(stash_prefix(&envelope.recipient_ipk.0));
    let (peers, self_in_k) = closest_homes_with_self(dht, &target);

    let mut homes_succeeded: usize = 0;

    if self_in_k {
        let req = WelcomePublishReq { envelope: envelope.clone(), timestamp };
        if handle_welcome_publish(dht, req, dht.node_id, now_ms) == WelcomePublishOutcome::Stored {
            homes_succeeded += 1;
        }
    }

    let req = DhtRequest::WelcomePublish(WelcomePublishReq { envelope, timestamp });
    for resp in fan_out_collect(dht, &peers, &req).await {
        if let DhtResponse::WelcomePublish(r) = resp
            && r.outcome == WelcomePublishOutcome::Stored
        {
            homes_succeeded += 1;
        }
    }

    homes_succeeded >= FORWARD_K_MIN
}

/// Originate a Welcome drain of the user's own queue. `sig` is the
/// phone's signature over `welcome_fetch_signing_input(user_ipk,
/// self.node_id, timestamp)`. Merges entries across all reachable homes
/// and dedupes by `(group_id, kp_ref_used)` — `welcome_id` differs per
/// replica so it cannot be the dedupe key.
pub(crate) async fn originate_welcome_fetch(
    dht: &Arc<Dht>, user_ipk: [u8; 32], timestamp: u64, sig: [u8; 64], now_ms: u64,
) -> Vec<WelcomeEntry> {
    let target = NodeId::from_bytes(stash_prefix(&user_ipk));
    let (peers, self_in_k) = closest_homes_with_self(dht, &target);

    let req = WelcomeFetchReq {
        user_ipk: user_ipk.into(),
        requester_relay_id: dht.node_id,
        timestamp,
        user_sig: sig.into(),
    };

    let mut seen: HashSet<([u8; 32], [u8; 32])> = HashSet::new();
    let mut merged: Vec<WelcomeEntry> = Vec::new();

    if self_in_k
        && let WelcomeFetchOutcome::Found(found) =
            handle_welcome_fetch(dht, req.clone(), dht.node_id, now_ms)
    {
        absorb(found.welcomes, &mut seen, &mut merged);
    }

    for resp in fan_out_collect(dht, &peers, &DhtRequest::WelcomeFetch(req.clone())).await {
        if let DhtResponse::WelcomeFetch(r) = resp
            && let WelcomeFetchOutcome::Found(found) = r.outcome
        {
            absorb(found.welcomes, &mut seen, &mut merged);
        }
    }

    merged
}

/// Merge `entries` into `merged`, deduping by `(group_id, kp_ref_used)`
/// against `seen`. A replica that double-stored a welcome under a
/// distinct `welcome_id` is collapsed to one entry here.
fn absorb(
    entries: Vec<WelcomeEntry>, seen: &mut HashSet<([u8; 32], [u8; 32])>,
    merged: &mut Vec<WelcomeEntry>,
) {
    for e in entries {
        let key = (e.envelope.group_id.0, e.envelope.kp_ref_used.0);
        if seen.insert(key) {
            merged.push(e);
        }
    }
}

/// Originate a Welcome ack — GC the listed `welcome_ids` from the
/// user's K homes. `sig` is the phone's signature over
/// `welcome_ack_signing_input(user_ipk, self.node_id, welcome_ids,
/// timestamp)`. Best-effort all-K (no quorum); an unacked id just
/// lingers until TTL and re-acks on the next reconnect.
pub(crate) async fn originate_welcome_ack(
    dht: &Arc<Dht>, user_ipk: [u8; 32], welcome_ids: Vec<[u8; 8]>, timestamp: u64,
    sig: [u8; 64], now_ms: u64,
) {
    let target = NodeId::from_bytes(stash_prefix(&user_ipk));
    let (peers, self_in_k) = closest_homes_with_self(dht, &target);

    let req = WelcomeAckReq {
        user_ipk: user_ipk.into(),
        requester_relay_id: dht.node_id,
        welcome_ids: welcome_ids.into_iter().map(Into::into).collect(),
        timestamp,
        user_sig: sig.into(),
    };

    if self_in_k {
        let _ = handle_welcome_ack(dht, req.clone(), dht.node_id, now_ms);
    }

    // Best-effort fan-out; outcomes are intentionally discarded.
    let _ = fan_out_collect(dht, &peers, &DhtRequest::WelcomeAck(req)).await;
}

// ---------------------------------------------------------------------------
// Tests — self-only fan-out (single relay, empty routing table). The
// remote multi-relay path is exercised by the e2e harness.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
    use common::proto::mls_wire::MLS_WIRE_VERSION;
    use common::proto::mls_wire::welcome_ack_signing_input;
    use common::proto::mls_wire::welcome_envelope_signing_input;
    use common::proto::mls_wire::welcome_fetch_signing_input;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;

    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        seed[16] = ((n >> 8) & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-mls-welcome-orig-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        Arc::new(Dht::new(self_id, signing, DhtConfig::default(), store).expect("dht"))
    }

    fn build_envelope(sender: &SigningKey, recipient_ipk: [u8; 32], blob: Vec<u8>) -> WelcomeEnvelopeP {
        let sender_ipk: [u8; 32] = sender.verifying_key().to_bytes();
        // Derive (group_id, kp_ref_used) from the blob so callers can mint
        // genuinely *distinct* welcomes — `absorb` dedupes on this pair, so a
        // fixed value would collapse two welcomes into one.
        let tag = blob.first().copied().unwrap_or(0);
        let group_id = [0xAA ^ tag; 32];
        let kp_ref_used = [0xBB ^ tag; 32];
        let msg = welcome_envelope_signing_input(
            MLS_WIRE_VERSION, &group_id, &sender_ipk, &recipient_ipk, &kp_ref_used, &blob,
        );
        let sig = sender.sign(&msg);
        WelcomeEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: group_id.into(),
            sender_ipk: sender_ipk.into(),
            recipient_ipk: recipient_ipk.into(),
            welcome_blob: blob.into(),
            kp_ref_used: kp_ref_used.into(),
            sender_sig: sig.to_bytes().into(),
        }
    }

    fn sign_fetch(user: &SigningKey, requester: &NodeId, timestamp: u64) -> [u8; 64] {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg = welcome_fetch_signing_input(MLS_WIRE_VERSION, &user_ipk, requester, timestamp);
        user.sign(&msg).to_bytes()
    }

    fn sign_ack(user: &SigningKey, requester: &NodeId, ids: &[[u8; 8]], timestamp: u64) -> [u8; 64] {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg = welcome_ack_signing_input(MLS_WIRE_VERSION, &user_ipk, requester, ids, timestamp);
        user.sign(&msg).to_bytes()
    }

    /// Self-only publish stores the welcome; an originate_fetch by the
    /// recipient (signed against our node_id) returns it; an ack then
    /// deletes it so a second fetch is empty.
    #[tokio::test(flavor = "current_thread")]
    async fn self_only_publish_fetch_ack_round_trip() {
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = 1_700_000_000_000;

        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();

        let envelope = build_envelope(&sender, recipient_ipk, b"welcome".to_vec());
        let _quorum = originate_welcome_publish(&dht, envelope, now, now).await;
        // Lone relay can't meet K_MIN, but the welcome is stored locally.

        let sig = sign_fetch(&recipient, &dht.node_id, now);
        let entries = originate_welcome_fetch(&dht, recipient_ipk, now, sig, now).await;
        assert_eq!(entries.len(), 1, "fetch should return the stored welcome");

        let ids: Vec<[u8; 8]> = entries.iter().map(|e| e.welcome_id.0).collect();
        let ack_sig = sign_ack(&recipient, &dht.node_id, &ids, now);
        originate_welcome_ack(&dht, recipient_ipk, ids, now, ack_sig, now).await;

        let sig2 = sign_fetch(&recipient, &dht.node_id, now);
        let after = originate_welcome_fetch(&dht, recipient_ipk, now, sig2, now).await;
        assert!(after.is_empty(), "ack should have deleted the welcome");
    }

    /// Fetch merges + dedupes by (group_id, kp_ref_used): two stored
    /// welcomes for the same recipient under distinct (group, kp_ref)
    /// both come back.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_returns_distinct_welcomes() {
        let dht = fresh_dht(NodeId::new([0u8; 32]));
        let now = 1_700_000_000_000;
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();

        for i in 0..2u8 {
            let sender = fresh_signing_key();
            let env = build_envelope(&sender, recipient_ipk, vec![i; 8]);
            originate_welcome_publish(&dht, env, now, now).await;
        }

        let sig = sign_fetch(&recipient, &dht.node_id, now);
        let entries = originate_welcome_fetch(&dht, recipient_ipk, now, sig, now).await;
        assert_eq!(entries.len(), 2);
    }
}
