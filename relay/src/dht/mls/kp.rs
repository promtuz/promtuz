//! KeyPackage stash RPCs at a home relay: publish, refill and the one-shot fetch.

use common::crypto::verify_ed25519;
use common::proto::mls_wire::KP_STASH_TARGET;
use common::proto::mls_wire::KeyPackageFetchFound;
use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackageFetchReq;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KeyPackageRefillOutcome;
use common::proto::mls_wire::KpPublishMode;
use common::proto::mls_wire::MAX_KP_FETCH_PER_HOUR;
use common::proto::mls_wire::MAX_KP_SKEW_MS;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::key_package_stash_prefix;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_record_signing_input;
use common::proto::mls_wire::kp_refill_signing_input;
use common::quic::id::NodeId;

use super::Reject;
use super::hourly_quota;
use super::stash_gate;
use crate::dht::Dht;
use crate::dht::rate_limit::KeyedLimiter;
use crate::dht::rate_limit::LimiterClock;
use crate::dht::rate_limit::keyed;
use crate::storage::key_packages;

/// A SHA-256 KeyPackageRef (RFC 9420 §5.2).
const KP_REF_LEN: usize = 32;

/// Keyed per `(target, requester)`, so a relay draining one stash keeps its quota for the others.
pub(crate) type KpFetchKey = ([u8; 32], [u8; 32]);

/// Caps one stash's depletion across all requesters. Relay identities are free to mint, so the
/// per-pair quota alone scales with the attacker's keypairs.
const MAX_KP_FETCH_PER_TARGET_PER_HOUR: u32 = 120;

#[derive(Debug)]
pub(crate) struct KpFetchLimiters {
    pub(crate) per_pair:   KeyedLimiter<KpFetchKey>,
    pub(crate) per_target: KeyedLimiter<[u8; 32]>,
}

impl KpFetchLimiters {
    pub(crate) fn new(clock: &LimiterClock) -> Self {
        Self {
            per_pair:   keyed(hourly_quota(MAX_KP_FETCH_PER_HOUR), clock),
            per_target: keyed(hourly_quota(MAX_KP_FETCH_PER_TARGET_PER_HOUR), clock),
        }
    }

    pub(crate) fn check(
        &self, target_ipk: &[u8; 32], requester: &NodeId,
    ) -> Result<(), ()> {
        let key: KpFetchKey = (*target_ipk, *requester.as_bytes());
        self.per_pair.check_key(&key).map_err(|_| ())?;
        self.per_target.check_key(target_ipk).map_err(|_| ())
    }

    pub(crate) fn sweep(&self) {
        self.per_pair.retain_recent();
        self.per_pair.shrink_to_fit();
        self.per_target.retain_recent();
        self.per_target.shrink_to_fit();
    }
}

fn verify_record(rec: &KeyPackageRecord, now_ms: u64) -> Result<(), KeyPackageVerifyError> {
    use common::proto::mls_wire::KEYPACKAGE_LIFETIME_MS;

    if rec.kp_ref.0.len() != KP_REF_LEN {
        return Err(KeyPackageVerifyError::Malformed);
    }
    if rec.kp_bytes.0.is_empty() {
        return Err(KeyPackageVerifyError::Malformed);
    }

    // The transcript covers `BLAKE3(kp_bytes)`, so the signature also binds the KeyPackage body.
    let msg = kp_record_signing_input(
        MLS_WIRE_VERSION,
        &rec.ipk.0,
        &rec.kp_ref.0,
        &rec.kp_bytes.0,
        rec.expires_at_ms,
    );
    verify_ed25519(&rec.ipk.0, &msg, &rec.owner_sig.0)
        .map_err(|_| KeyPackageVerifyError::BadSig)?;

    if rec.expires_at_ms <= now_ms {
        return Err(KeyPackageVerifyError::Expired);
    }

    // A far-future expiry would defeat rotation; the skew allowance covers a fast clock.
    let max_expiry = now_ms
        .saturating_add(KEYPACKAGE_LIFETIME_MS)
        .saturating_add(MAX_KP_SKEW_MS);
    if rec.expires_at_ms > max_expiry {
        return Err(KeyPackageVerifyError::Malformed);
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum KeyPackageVerifyError {
    Malformed,
    BadSig,
    Expired,
}

fn verify_outer_sig(
    publisher_ipk: &[u8; 32], outer_sig: &[u8; 64], records: &[KeyPackageRecord],
    timestamp: u64, mode: KpPublishMode, now_ms: u64,
) -> bool {
    let skew = now_ms.abs_diff(timestamp);
    if skew > MAX_KP_SKEW_MS {
        return false;
    }

    let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
    let count = records.len() as u32;
    let msg = match mode {
        KpPublishMode::Publish => {
            kp_publish_signing_input(MLS_WIRE_VERSION, publisher_ipk, &digest, count, timestamp)
        },
        KpPublishMode::Refill => {
            kp_refill_signing_input(MLS_WIRE_VERSION, publisher_ipk, &digest, count, timestamp)
        },
    };
    verify_ed25519(publisher_ipk, &msg, outer_sig).is_ok()
}

/// Four bytes of the IPK, so logs do not expose the full contact graph.
fn fmt_ipk(ipk: &[u8; 32]) -> String {
    hex::encode(&ipk[..4])
}

/// A fetch pops the record closest to this point. The one-shot guarantee is per replica, so a fixed
/// order would hand two senders the same KeyPackage; a requester-derived point keeps them apart.
fn pop_selector(requester: &NodeId, target_ipk: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(requester.as_bytes());
    buf.extend_from_slice(target_ipk);
    *NodeId::new(&buf).as_bytes()
}

/// `BLAKE3(target_ipk || kp_ref || BLAKE3(kp_bytes))`, comparable across replicas to spot a
/// substituted body. Defense in depth only: the client verifies `owner_sig`, which covers the body.
fn compute_static_hash(rec: &KeyPackageRecord) -> [u8; 32] {
    let kp_bytes_digest = *NodeId::new(&rec.kp_bytes.0).as_bytes();
    let mut buf = Vec::with_capacity(32 + rec.kp_ref.0.len() + 32);
    buf.extend_from_slice(&rec.ipk.0);
    buf.extend_from_slice(&rec.kp_ref.0);
    buf.extend_from_slice(&kp_bytes_digest);
    *NodeId::new(&buf).as_bytes()
}

/// Publish replaces the stash and refill appends to it. `Stored` is a durable promise, so it waits
/// for the persist barrier.
pub(crate) async fn handle_keypackage_publish(
    dht: &Dht, ipk: &[u8; 32], records: &[KeyPackageRecord], timestamp: u64, sig: &[u8; 64],
    mode: KpPublishMode, now_ms: u64,
) -> KeyPackagePublishOutcome {
    use KeyPackagePublishOutcome as Outcome;

    if records.len() > KP_STASH_TARGET {
        return Outcome::TooMany;
    }
    if !verify_outer_sig(ipk, sig, records, timestamp, mode, now_ms) {
        return Outcome::BadSig;
    }
    if !crate::dht::routing::homes(dht, &NodeId::from_bytes(key_package_stash_prefix(ipk))).1 {
        return Outcome::NotOwner;
    }
    for rec in records {
        if rec.ipk.0 != *ipk {
            return Outcome::BadSig;
        }
        match verify_record(rec, now_ms) {
            Ok(()) => {},
            Err(KeyPackageVerifyError::Expired) => return Outcome::Expired,
            Err(_) => return Outcome::BadSig,
        }
    }

    match dht.store.key_packages.publish(ipk, records, mode == KpPublishMode::Publish, now_ms) {
        Ok(()) => {},
        Err(key_packages::Error::Conflict) => return Outcome::StaticFieldsConflict,
        Err(key_packages::Error::Full) => return Outcome::TooMany,
        Err(error) => {
            common::warn!("key package {mode:?} failed: {error}");
            return Outcome::Unavailable;
        },
    }
    if dht.store.persist_barrier().wait().await.is_err() {
        return Outcome::Unavailable;
    }
    Outcome::Stored
}

/// A refill answers with the publish outcomes, `Appended` standing for `Stored`.
pub(crate) fn refill_outcome(outcome: KeyPackagePublishOutcome) -> KeyPackageRefillOutcome {
    use KeyPackagePublishOutcome as P;
    use KeyPackageRefillOutcome as R;
    match outcome {
        P::Stored => R::Appended,
        P::BadSig => R::BadSig,
        P::Expired => R::Expired,
        P::NotOwner => R::NotOwner,
        P::RateLimited => R::RateLimited,
        P::TooMany => R::TooMany,
        P::StaticFieldsConflict => R::StaticFieldsConflict,
        P::Unavailable => R::Unavailable,
    }
}

/// A consumed record is not released to a requester until both its removal and the
/// anti-republication marker survive a machine crash.
pub(crate) async fn handle_keypackage_fetch(
    dht: &Dht, req: KeyPackageFetchReq, peer: NodeId, now_ms: u64,
) -> KeyPackageFetchOutcome {
    let target = req.target_ipk.0;
    let admit = || dht.kp_fetch_limiters.check(&target, &req.requester_relay_id).is_ok();
    let stash = key_package_stash_prefix(&target);
    if let Err(reject) =
        stash_gate(dht, peer, Some(req.requester_relay_id), req.timestamp, now_ms, stash, admit)
    {
        common::debug!("MLS kp_fetch: {reject:?} for target_ipk={}", fmt_ipk(&target));
        // Everything but ownership answers `RateLimited`, so a probe cannot learn whether the user
        // has a stash.
        return match reject {
            Reject::NotOwner => KeyPackageFetchOutcome::NotOwner,
            _ => KeyPackageFetchOutcome::RateLimited,
        };
    }

    let selector = pop_selector(&req.requester_relay_id, &target);
    let outcome = match dht.store.key_packages.take(&target, &selector, now_ms) {
        Ok(Some((record, remaining))) => {
            let static_hash = compute_static_hash(&record).into();
            KeyPackageFetchOutcome::Found(KeyPackageFetchFound { record, remaining, static_hash })
        },
        Ok(None) => KeyPackageFetchOutcome::NoStash,
        Err(error) => {
            common::warn!("key package consumption failed: {error}");
            KeyPackageFetchOutcome::Unavailable
        },
    };
    if matches!(outcome, KeyPackageFetchOutcome::Found(_))
        && dht.store.persist_barrier().wait().await.is_err()
    {
        return KeyPackageFetchOutcome::Unavailable;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use common::utils::now_ms;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::test_support::dht;
    use crate::test_support::key;
    use crate::test_support::kp_record;
    use crate::test_support::kp_sig;

    const HOUR: u64 = 3_600_000;

    async fn publish(
        dht: &Dht, owner: &SigningKey, records: &[KeyPackageRecord], mode: KpPublishMode,
        signed_at: u64, now: u64,
    ) -> KeyPackagePublishOutcome {
        let sig = kp_sig(owner, records, mode, signed_at);
        let ipk = owner.verifying_key().to_bytes();
        handle_keypackage_publish(dht, &ipk, records, signed_at, &sig, mode, now).await
    }

    async fn fetch(
        dht: &Dht, owner: &SigningKey, requester: NodeId, peer: NodeId, now: u64,
    ) -> KeyPackageFetchOutcome {
        let req = KeyPackageFetchReq {
            target_ipk:         owner.verifying_key().to_bytes().into(),
            requester_relay_id: requester,
            timestamp:          now,
        };
        handle_keypackage_fetch(dht, req, peer, now).await
    }

    fn stash(dht: &Dht, owner: &SigningKey, now: u64) -> Vec<u8> {
        let refs =
            dht.store.key_packages.inventory(&owner.verifying_key().to_bytes(), now).unwrap();
        refs.iter().map(|kp_ref| kp_ref[0]).collect()
    }

    fn records(owner: &SigningKey, refs: std::ops::Range<u8>, now: u64) -> Vec<KeyPackageRecord> {
        refs.map(|n| kp_record(owner, [n; 32], now + HOUR)).collect()
    }

    /// One-shot custody: each fetch vends a different package, and none twice.
    #[tokio::test]
    async fn each_fetch_vends_a_different_package_exactly_once() {
        let (_dir, dht) = dht(NodeId::from_bytes([0; 32]));
        let (owner, peer, now) = (key(1), NodeId::from_bytes([2; 32]), now_ms());
        let stored =
            publish(&dht, &owner, &records(&owner, 1..4, now), KpPublishMode::Publish, now, now)
                .await;
        assert_eq!(stored, KeyPackagePublishOutcome::Stored);
        let mut vended = Vec::new();
        for remaining in [2, 1, 0] {
            let KeyPackageFetchOutcome::Found(found) = fetch(&dht, &owner, peer, peer, now).await
            else {
                panic!("expected a package with {remaining} left");
            };
            assert_eq!(found.remaining, remaining);
            vended.push(found.record.kp_ref.0[0]);
        }
        vended.sort();
        assert_eq!(vended, [1, 2, 3]);
        assert_eq!(fetch(&dht, &owner, peer, peer, now).await, KeyPackageFetchOutcome::NoStash);
    }

    /// Replicas choose by requester, so two senders asking different homes for the same user do
    /// not both receive the one package.
    #[tokio::test]
    async fn replicas_vend_different_packages_to_different_requesters() {
        let (owner, now) = (key(1), now_ms());
        let all = records(&owner, 0..16, now);
        let mut vended = Vec::new();
        for requester in [[0xA1; 32], [0xB2; 32]] {
            let (_dir, replica) = dht(NodeId::from_bytes([0; 32]));
            let requester = NodeId::from_bytes(requester);
            publish(&replica, &owner, &all, KpPublishMode::Publish, now, now).await;
            let KeyPackageFetchOutcome::Found(found) =
                fetch(&replica, &owner, requester, requester, now).await
            else {
                panic!("expected a package");
            };
            vended.push(found.record.kp_ref);
        }
        assert_ne!(vended[0], vended[1]);
    }

    /// A stash changes only by a fresh, owner-signed, consistent snapshot. Rows: operation,
    /// outcome, then the stash after it.
    #[tokio::test]
    async fn a_stash_changes_only_by_a_signed_fresh_consistent_publication() {
        let (_dir, dht) = dht(NodeId::from_bytes([0; 32]));
        let (owner, now) = (key(1), now_ms());
        let [a, b, c] = [1, 2, 3].map(|n| kp_record(&owner, [n; 32], now + HOUR));
        let mut forged_record = a.clone();
        forged_record.owner_sig.0[0] ^= 1;
        let mut changed_a = kp_record(&owner, [1; 32], now + HOUR);
        changed_a.kp_bytes.0.push(0);
        changed_a = KeyPackageRecord { owner_sig: kp_record_sig(&owner, &changed_a), ..changed_a };
        let expired = kp_record(&owner, [4; 32], now);
        let full = records(&owner, 10..10 + KP_STASH_TARGET as u8, now);
        let too_many = records(&owner, 10..11 + KP_STASH_TARGET as u8, now);
        let (publish_, refill) = (KpPublishMode::Publish, KpPublishMode::Refill);

        use KeyPackagePublishOutcome::*;
        let filled: Vec<u8> = (10..10 + KP_STASH_TARGET as u8).collect();
        let check = |label: &str, outcome, expected, after: &[u8]| {
            assert_eq!(outcome, expected, "{label}");
            let mut found = stash(&dht, &owner, now);
            found.sort();
            assert_eq!(found, after, "{label}");
        };
        check(
            "forged record",
            publish(&dht, &owner, &[forged_record], publish_, now, now).await,
            BadSig,
            &[],
        );
        check(
            "stale signature",
            publish(&dht, &owner, &[a.clone()], publish_, now - 120_000, now).await,
            BadSig,
            &[],
        );
        check(
            "expired record",
            publish(&dht, &owner, &[expired], publish_, now, now).await,
            Expired,
            &[],
        );
        check(
            "past the target",
            publish(&dht, &owner, &too_many, publish_, now, now).await,
            TooMany,
            &[],
        );
        check(
            "publish",
            publish(&dht, &owner, &[a.clone()], publish_, now, now).await,
            Stored,
            &[1],
        );
        check(
            "same ref, other bytes",
            publish(&dht, &owner, &[changed_a.clone()], publish_, now, now).await,
            StaticFieldsConflict,
            &[1],
        );
        check(
            "refill, same ref, other bytes",
            publish(&dht, &owner, &[changed_a], refill, now, now).await,
            StaticFieldsConflict,
            &[1],
        );
        check("refill", publish(&dht, &owner, &[b], refill, now, now).await, Stored, &[1, 2]);
        check(
            "publish replaces",
            publish(&dht, &owner, &[c], publish_, now, now).await,
            Stored,
            &[3],
        );
        check("fill", publish(&dht, &owner, &full, publish_, now, now).await, Stored, &filled);
        check(
            "refill past the target",
            publish(&dht, &owner, &[a], refill, now, now).await,
            TooMany,
            &filled,
        );
        check(
            "idempotent resend",
            publish(&dht, &owner, &full[..1], refill, now, now).await,
            Stored,
            &filled,
        );

        // Only an owner-signed empty snapshot withdraws the last packages.
        let mut forged_empty = kp_sig(&owner, &[], publish_, now);
        forged_empty[0] ^= 1;
        let ipk = owner.verifying_key().to_bytes();
        assert_eq!(
            handle_keypackage_publish(&dht, &ipk, &[], now, &forged_empty, publish_, now).await,
            BadSig
        );
        assert_eq!(stash(&dht, &owner, now).len(), KP_STASH_TARGET);
        assert_eq!(publish(&dht, &owner, &[], publish_, now, now).await, Stored);
        assert!(stash(&dht, &owner, now).is_empty());

        // A fetch captured from one relay and replayed by another gets nothing.
        let (signer, replayer) = (NodeId::from_bytes([0xA1; 32]), NodeId::from_bytes([0xB2; 32]));
        assert_eq!(
            fetch(&dht, &owner, signer, replayer, now).await,
            KeyPackageFetchOutcome::RateLimited
        );
    }

    fn kp_record_sig(
        owner: &SigningKey, record: &KeyPackageRecord,
    ) -> common::types::bytes::Bytes<64> {
        use ed25519_dalek::Signer;
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &record.ipk.0,
            &record.kp_ref.0,
            &record.kp_bytes.0,
            record.expires_at_ms,
        );
        owner.sign(&msg).to_bytes().into()
    }
}
