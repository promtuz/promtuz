//! Tier-2 OUTBOUND fan-out for KeyPackage publish / refill / fetch.
//! The relay-side analogue of `forward.rs::forward_to_homes`,
//! specialised to the MLS KeyPackage RPCs.
//!
//! A phone delegates a KP operation to its home relay over the
//! `client/0` wrappers; the home originates the real `peer/1` fan-out to
//! the target's K-closest homes, self-storing if it is itself a home.
//! The phone's *inner* user signature
//! ([`kp_publish_signing_input`](common::proto::mls_wire::kp_publish_signing_input)
//! / refill) is forwarded verbatim — the home cannot forge it, so the K
//! storage homes verify the user exactly as on a relay-to-relay dial.
//!
//! KeyPackage *fetch* carries no user signature (it is DhtHello-
//! authenticated relay-to-relay) and consumes a one-shot slot at every
//! home it reaches, so the fetch path dials sequentially and stops at
//! the first `Found` rather than fanning out (which would burn K slots).

use std::sync::Arc;

use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackageFetchReq;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackagePublishReq;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KeyPackageRefillOutcome;
use common::proto::mls_wire::KeyPackageRefillReq;
use common::proto::mls_wire::KpPublishMode;
use common::quic::id::NodeId;

use super::Dht;
use super::config::FORWARD_K_MIN;
use super::mls_fanout::closest_homes_with_self;
use super::mls_fanout::fan_out_collect;
use super::mls_fanout::remote_rpc_one;
use super::mls_kp::handle_keypackage_fetch;
use super::mls_kp::handle_keypackage_publish;
use super::mls_kp::handle_keypackage_refill;
use super::mls_kp::stash_prefix;

/// Outcome of a publish / refill fan-out.
pub(crate) struct KpPublishQuorum {
    /// Homes (incl. self) that returned a success outcome — `Stored`
    /// for Publish, `Appended` for Refill.
    pub homes_succeeded: u8,
    /// `homes_succeeded >= FORWARD_K_MIN` (= 2).
    pub quorum_met: bool,
}

/// Outcome of a fetch fan-out (any-of-K first success).
pub(crate) struct KpFetchResult {
    /// The fetched record, or `None` if no reachable home held an
    /// in-lifetime KP for the target (collapses `NoStash` / `NotOwner`).
    pub record: Option<KeyPackageRecord>,
    /// Home's stash size after this fetch (0 when `record` is `None`).
    pub remaining: u32,
    /// Cross-replica static-fields hash (zeros when `record` is `None`).
    pub static_hash: [u8; 32],
}

/// Originate a KeyPackage publish (`mode = Publish`) or refill
/// (`mode = Refill`) on the phone's behalf. `sig` is the phone's
/// signature over the inner Tier-2 transcript (selected by `mode`),
/// forwarded verbatim to every home.
pub(crate) async fn originate_publish(
    dht: &Arc<Dht>, ipk: [u8; 32], records: Vec<KeyPackageRecord>, mode: KpPublishMode,
    timestamp: u64, sig: [u8; 64], now_ms: u64,
) -> KpPublishQuorum {
    let target = NodeId::from_bytes(stash_prefix(&ipk));
    let (peers, self_in_k) = closest_homes_with_self(dht, &target);

    let mut homes_succeeded: u8 = 0;

    // Self-store shortcut — invoke the inbound handler directly with
    // our own node_id as the authenticated peer (no network hop, no
    // circular dial). The handler re-verifies the phone's sig.
    if self_in_k {
        let stored = match mode {
            KpPublishMode::Publish => matches!(
                handle_keypackage_publish(dht, build_publish_req(ipk, records.clone(), timestamp, sig), dht.node_id, now_ms),
                KeyPackagePublishOutcome::Stored
            ),
            KpPublishMode::Refill => matches!(
                handle_keypackage_refill(dht, build_refill_req(ipk, records.clone(), timestamp, sig), dht.node_id, now_ms),
                KeyPackageRefillOutcome::Appended
            ),
        };
        if stored {
            homes_succeeded += 1;
        }
    }

    // Remote fan-out (parallel quorum).
    let req = match mode {
        KpPublishMode::Publish => {
            DhtRequest::KeyPackagePublish(build_publish_req(ipk, records, timestamp, sig))
        },
        KpPublishMode::Refill => {
            DhtRequest::KeyPackageRefill(build_refill_req(ipk, records, timestamp, sig))
        },
    };
    for resp in fan_out_collect(dht, &peers, &req).await {
        let success = match (&mode, resp) {
            (KpPublishMode::Publish, DhtResponse::KeyPackagePublish(r)) => {
                r.outcome == KeyPackagePublishOutcome::Stored
            },
            (KpPublishMode::Refill, DhtResponse::KeyPackageRefill(r)) => {
                r.outcome == KeyPackageRefillOutcome::Appended
            },
            _ => false,
        };
        if success {
            homes_succeeded = homes_succeeded.saturating_add(1);
        }
    }

    KpPublishQuorum {
        homes_succeeded,
        quorum_met: (homes_succeeded as usize) >= FORWARD_K_MIN,
    }
}

/// Originate a KeyPackage fetch for `target_ipk`. Tries self first
/// (no slot lost to the network), then each remote home sequentially,
/// stopping at the first `Found` — a fetch consumes a one-shot slot, so
/// fanning out would waste KPs. `requester_relay_id` is our own
/// node_id; the remote home's DhtHello handshake authenticates it.
pub(crate) async fn originate_fetch(
    dht: &Arc<Dht>, target_ipk: [u8; 32], now_ms: u64,
) -> KpFetchResult {
    let target = NodeId::from_bytes(stash_prefix(&target_ipk));
    let (peers, self_in_k) = closest_homes_with_self(dht, &target);

    let req = KeyPackageFetchReq {
        target_ipk: target_ipk.into(),
        requester_relay_id: dht.node_id,
        timestamp: now_ms,
    };

    if self_in_k
        && let KeyPackageFetchOutcome::Found(f) =
            handle_keypackage_fetch(dht, req.clone(), dht.node_id, now_ms)
    {
        return KpFetchResult {
            record: Some(f.record),
            remaining: f.remaining,
            static_hash: f.static_hash.0,
        };
    }

    for peer in &peers {
        if let Some(DhtResponse::KeyPackageFetch(resp)) =
            remote_rpc_one(dht, peer, &DhtRequest::KeyPackageFetch(req.clone())).await
            && let KeyPackageFetchOutcome::Found(f) = resp.outcome
        {
            return KpFetchResult {
                record: Some(f.record),
                remaining: f.remaining,
                static_hash: f.static_hash.0,
            };
        }
    }

    KpFetchResult { record: None, remaining: 0, static_hash: [0u8; 32] }
}

fn build_publish_req(
    ipk: [u8; 32], records: Vec<KeyPackageRecord>, timestamp: u64, sig: [u8; 64],
) -> KeyPackagePublishReq {
    KeyPackagePublishReq { ipk: ipk.into(), records, timestamp, sig: sig.into() }
}

fn build_refill_req(
    ipk: [u8; 32], records: Vec<KeyPackageRecord>, timestamp: u64, sig: [u8; 64],
) -> KeyPackageRefillReq {
    KeyPackageRefillReq { ipk: ipk.into(), records, timestamp, sig: sig.into() }
}

// ---------------------------------------------------------------------------
// Tests — self-only fan-out (single relay, empty routing table). The
// remote multi-relay path is exercised by the e2e harness.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::mls_wire::KEYPACKAGE_LIFETIME_MS;
    use common::proto::mls_wire::MLS_WIRE_VERSION;
    use common::proto::mls_wire::kp_publish_records_digest;
    use common::proto::mls_wire::kp_publish_signing_input;
    use common::proto::mls_wire::kp_record_signing_input;
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
        let path = std::env::temp_dir().join(format!("promtuz-mls-kp-orig-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        Arc::new(Dht::new(self_id, signing, DhtConfig::default(), store).expect("dht"))
    }

    fn build_record(owner: &SigningKey, kp_ref: [u8; 32], now_ms: u64) -> KeyPackageRecord {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let kp_bytes = vec![0xCDu8; 16];
        let expires_at_ms = now_ms + KEYPACKAGE_LIFETIME_MS;
        let msg = kp_record_signing_input(MLS_WIRE_VERSION, &ipk, &kp_ref, &kp_bytes, expires_at_ms);
        let sig = owner.sign(&msg);
        KeyPackageRecord {
            ipk: ipk.into(),
            kp_ref: kp_ref.to_vec().into(),
            kp_bytes: kp_bytes.into(),
            expires_at_ms,
            owner_sig: sig.to_bytes().into(),
        }
    }

    /// Sign the inner Publish transcript exactly as the phone would.
    fn sign_publish(owner: &SigningKey, records: &[KeyPackageRecord], timestamp: u64) -> [u8; 64] {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
        let msg =
            kp_publish_signing_input(MLS_WIRE_VERSION, &ipk, &digest, records.len() as u32, timestamp);
        owner.sign(&msg).to_bytes()
    }

    /// Self-only publish stores the batch (1 of K), and a subsequent
    /// originate_fetch returns the stored record. Quorum is NOT met with
    /// a lone relay (FORWARD_K_MIN = 2), which is the correct signal.
    #[tokio::test(flavor = "current_thread")]
    async fn self_only_publish_then_fetch_round_trip() {
        let dht = fresh_dht(NodeId::new([0u8; 32]));
        let now = 1_700_000_000_000;
        let owner = fresh_signing_key();
        let owner_ipk: [u8; 32] = owner.verifying_key().to_bytes();

        let records = vec![build_record(&owner, [0x11; 32], now)];
        let sig = sign_publish(&owner, &records, now);

        let q = originate_publish(
            &dht, owner_ipk, records, KpPublishMode::Publish, now, sig, now,
        )
        .await;
        assert_eq!(q.homes_succeeded, 1, "self should self-store");
        assert!(!q.quorum_met, "a lone relay cannot meet K_MIN = 2");

        let fetched = originate_fetch(&dht, owner_ipk, now).await;
        assert!(fetched.record.is_some(), "fetch should return the stored KP");
        assert_eq!(fetched.record.unwrap().kp_ref.0, [0x11; 32].to_vec());
    }

    /// A fetch against a target with no stash returns `None`, not a
    /// panic or a bogus record.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_empty_stash_returns_none() {
        let dht = fresh_dht(NodeId::new([0u8; 32]));
        let now = 1_700_000_000_000;
        let stranger = fresh_signing_key();
        let stranger_ipk: [u8; 32] = stranger.verifying_key().to_bytes();

        let fetched = originate_fetch(&dht, stranger_ipk, now).await;
        assert!(fetched.record.is_none());
        assert_eq!(fetched.remaining, 0);
        assert_eq!(fetched.static_hash, [0u8; 32]);
    }
}
