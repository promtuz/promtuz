//! MLS KeyPackage stash storage and RPC handlers.
//!
//! Owns the `dht_keypackage` keyspace plus the three home-
//! relay handlers wired into [`super::handler::handle_dht_request`]:
//!
//! - [`handle_keypackage_publish`] — owner pushes a fresh batch of
//!   one-time KeyPackages.
//! - [`handle_keypackage_fetch`]  — sender-relay pops one KP from a
//!   target's stash (strict one-shot, FIFO order).
//! - [`handle_keypackage_refill`] — owner appends KPs incrementally
//!   when their stash dipped below the low-water mark.
//!
//! ## Storage layout
//!
//! The DHT key for a stash is `BLAKE3("kp:" || ipk)` (32 bytes). To
//! keep a *single* stash representable as multiple KP
//! rows in fjall while still benefiting from fjall's prefix-
//! iterator API, the on-disk key is:
//!
//! ```text
//!   stash_prefix(32) || kp_ref(32)   = 64 bytes
//!   stash_prefix    = BLAKE3("kp:" || ipk)
//!   kp_ref          = MLS KeyPackageRef (SHA-256 of TLS-encoded KP, 32 B)
//! ```
//!
//! The 32-byte fixed prefix lets us walk all KPs for a single user
//! via fjall's exact `prefix()` (same idiom as the queue keyspace). This also
//! makes the "consume one in FIFO order" path natural: scan, pop the
//! first matching record, delete it, return it.
//!
//! Value layout: postcard-encoded
//! [`common::proto::mls_wire::KeyPackageRecord`].
//!
//! ## Cross-replica static-fields check
//!
//! The wire spec calls for the home to detect the case where a
//! republish for an existing `(ipk, kp_ref)` carries **different**
//! `kp_bytes` than the on-disk record. That indicates a forgery /
//! replay attempt: KP_ref is `SHA-256(kp_bytes)` per RFC 9420 §5.2,
//! so a legitimate publisher cannot produce a different
//! `(ipk, kp_ref, kp_bytes)` triple — only an attacker substituting
//! a forged record can. We reject that with
//! [`KeyPackagePublishOutcome::StaticFieldsConflict`].
//!
//! Idempotent re-publish (byte-identical record) is allowed and is
//! a no-op (no rewrite to fjall).
//!
//! ## Anti-pinning rate limit
//!
//! `MAX_KP_FETCH_PER_HOUR = 60` is enforced per
//! `(target_ipk, requester_relay_id)` pair via a dedicated
//! `governor::RateLimiter` (separate from
//! [`super::rate_limit::PerPeerLimiters`], which is keyed only on
//! the requester). A misbehaving relay can drain Bob's stash 60×/hour
//! at *one* home; spreading across K=3 relays gives 180×/hour
//! aggregate, which is bounded by the relay PKI tier costs (Sybil
//! relays must be paid for).
//!
//! ## Lock contract
//!
//! All fjall I/O is sync; no `await` lives in this module's hot
//! paths. `parking_lot` discipline does not apply (we hold no
//! `parking_lot` guards here). The rate limiter is internally
//! lock-free (`governor`'s default keyed state store is DashMap-
//! backed).
//!

use std::num::NonZeroU32;
use std::sync::Arc;

use common::proto::mls_wire::KeyPackageFetchFound;
use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackageFetchReq;
use common::proto::mls_wire::KeyPackageFetchResp;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackagePublishReq;
use common::proto::mls_wire::KeyPackagePublishResp;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KeyPackageRefillOutcome;
use common::proto::mls_wire::KeyPackageRefillReq;
use common::proto::mls_wire::KeyPackageRefillResp;
use common::proto::mls_wire::MAX_KP_FETCH_PER_HOUR;
use common::proto::mls_wire::MAX_KP_SKEW_MS;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_record_signing_input;
use common::proto::mls_wire::kp_refill_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;

use super::Dht;

/// Length of the per-record SHA-256 KeyPackageRef, in bytes. Per
/// RFC 9420 §5.2 (cipher suite `0x0003`).
const KP_REF_LEN: usize = 32;

/// Length of the BLAKE3 stash-prefix, in bytes.
const STASH_PREFIX_LEN: usize = 32;

/// Fixed on-disk key length: stash_prefix(32) || kp_ref(32). Lookups scan
/// the `STASH_PREFIX_LEN`-byte prefix via fjall's exact `prefix()`.
const STORAGE_KEY_LEN: usize = STASH_PREFIX_LEN + KP_REF_LEN;

// ---------------------------------------------------------------------------
// Helpers — key construction
// ---------------------------------------------------------------------------

/// Compute the 32-byte stash prefix for `ipk`.
///
/// `BLAKE3("kp:" || ipk)`. The literal three-byte `"kp:"` prefix
/// differentiates KP-stash routing from presence routing (which uses
/// bare `ipk`) so the two namespaces don't share the same DHT key.
///
/// One-line wrapper that defers to the canonical
/// [`super::key_helpers::stash_prefix`] helper.
pub fn stash_prefix(ipk: &[u8; 32]) -> [u8; STASH_PREFIX_LEN] {
    super::key_helpers::stash_prefix(b"kp:", ipk)
}

/// Compute the on-disk `(stash_prefix || kp_ref)` storage key. Returns
/// `None` if `kp_ref.len() != KP_REF_LEN`.
fn storage_key(ipk: &[u8; 32], kp_ref: &[u8]) -> Option<[u8; STORAGE_KEY_LEN]> {
    if kp_ref.len() != KP_REF_LEN {
        return None;
    }
    let mut k = [0u8; STORAGE_KEY_LEN];
    k[..STASH_PREFIX_LEN].copy_from_slice(&stash_prefix(ipk));
    k[STASH_PREFIX_LEN..].copy_from_slice(kp_ref);
    Some(k)
}

// ---------------------------------------------------------------------------
// Per-pair rate limiter
// ---------------------------------------------------------------------------

/// Per-`(target_ipk, requester_relay_id)` quota for `KeyPackageFetch`.
///
/// Distinct from [`super::rate_limit::PerPeerLimiters`] (which is
/// keyed on requester alone): a misbehaving relay can drain Bob's
/// stash 60×/hour but is still allowed to legitimately fetch from
/// Alice's stash at the full quota in parallel.
///
/// The key shape is `(target_ipk_bytes, requester_node_id_bytes)`
/// flattened to a fixed-width tuple type. `governor`'s
/// `DefaultKeyedStateStore` is internally a DashMap that evicts idle
/// entries automatically, so the limiter doesn't grow unboundedly
/// over churn.
pub(crate) type KpFetchKey = ([u8; 32], [u8; 32]);

type KpFetchLimiter =
    RateLimiter<KpFetchKey, DefaultKeyedStateStore<KpFetchKey>, DefaultClock>;

/// Per-pair fetch limiter wrapper. One global instance lives on
/// [`Dht::mls_kp`].
///
/// Quota is `MAX_KP_FETCH_PER_HOUR` per pair; `governor` requires a
/// "per second" rate — we synthesise one by dividing per-hour by
/// 3600. Burst is set equal to the per-hour quota so a bursty client
/// (e.g. issuing 60 fetches in one second after a long quiet period)
/// doesn't trip until the burst-bucket is depleted.
#[derive(Debug)]
pub(crate) struct KpFetchLimiters {
    limiter: KpFetchLimiter,
}

impl KpFetchLimiters {
    pub(crate) fn new() -> Self {
        // 60 / hour → 60 tokens per 3600 s. governor::Quota does not
        // accept fractional rates directly; the cleanest expression
        // is `Quota::with_period(period_per_token).allow_burst(burst)`.
        let period = std::time::Duration::from_secs(3600 / MAX_KP_FETCH_PER_HOUR as u64);
        let burst = NonZeroU32::new(MAX_KP_FETCH_PER_HOUR)
            .unwrap_or(NonZeroU32::MIN);
        let quota = Quota::with_period(period)
            .expect("non-zero period per token")
            .allow_burst(burst);
        Self {
            limiter: RateLimiter::keyed(quota),
        }
    }

    /// Returns `Ok(())` if a token was consumed for `(target_ipk,
    /// requester)`, `Err(())` if the per-pair quota is exhausted.
    pub(crate) fn check(
        &self, target_ipk: &[u8; 32], requester: &NodeId,
    ) -> Result<(), ()> {
        let key: KpFetchKey = (*target_ipk, *requester.as_bytes());
        self.limiter.check_key(&key).map_err(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Self-is-owner helpers (mirror of forward.rs / store.rs)
// ---------------------------------------------------------------------------

/// True iff this relay is among the K closest to the *KP stash key*
/// (`stash_prefix(ipk)`) by current routing-table view.
///
/// Note: this is *not* the same as `self_is_owner` for presence
/// records (which uses bare `ipk` as the DHT key). KP routing keys
/// off the BLAKE3-prefixed hash to keep the DHT-routing model
/// uniform; operationally the two paths are independent.
///
/// One-line wrapper that defers to the canonical
/// [`super::routing::self_in_top_k`] helper.
fn self_is_owner_for_stash(dht: &Dht, ipk: &[u8; 32]) -> bool {
    super::routing::self_in_top_k(dht, &NodeId::from_bytes(stash_prefix(ipk)))
}

// ---------------------------------------------------------------------------
// Per-record verification
// ---------------------------------------------------------------------------

/// Verify a `KeyPackageRecord`'s shape, lifetime, and `owner_sig`.
///
/// Steps:
/// 1. Structural: `kp_ref.len() == 32`, `kp_bytes` non-empty.
/// 2. Owner-IPK shape: parses as Ed25519 verifying key.
/// 3. Owner sig verifies under `record.ipk` over
///    `kp_record_signing_input(MLS_WIRE_VERSION, ipk, kp_ref, expires_at_ms)`.
/// 4. Lifetime: `expires_at_ms > now_ms`.
/// 5. Lifetime upper bound:
///    `expires_at_ms <= now_ms + KEYPACKAGE_LIFETIME_MS + MAX_KP_SKEW_MS`
///    — defends against a publisher minting a 1000-year KP that
///    effectively never expires (would defeat anti-pinning rotation).
///
/// Returns `Ok(())` on success or a [`KeyPackageVerifyError`].
///
/// **Why a separate function**: Publish, Refill, and the read path
/// (Fetch) all need to validate records with the same discipline.
/// Centralising avoids drift; tests run against the function once
/// rather than three times.
fn verify_record(rec: &KeyPackageRecord, now_ms: u64) -> Result<(), KeyPackageVerifyError> {
    use common::proto::mls_wire::KEYPACKAGE_LIFETIME_MS;

    // 1. Structural.
    if rec.kp_ref.0.len() != KP_REF_LEN {
        return Err(KeyPackageVerifyError::Malformed);
    }
    if rec.kp_bytes.0.is_empty() {
        return Err(KeyPackageVerifyError::Malformed);
    }

    // 2. Owner pubkey shape.
    let vk = VerifyingKey::from_bytes(&rec.ipk.0)
        .map_err(|_| KeyPackageVerifyError::Malformed)?;

    // 3. Owner sig verify.
    //
    // The transcript folds in `BLAKE3(kp_bytes)`, so re-deriving and
    // matching `rec.kp_bytes` against the signed transcript is implicit
    // in `verify_strict` succeeding. A stolen IPK can no longer mint
    // `(ipk, kp_ref, fake_kp_bytes)` triples.
    let sig = Signature::from_bytes(&rec.owner_sig.0);
    let msg = kp_record_signing_input(
        MLS_WIRE_VERSION,
        &rec.ipk.0,
        &rec.kp_ref.0,
        &rec.kp_bytes.0,
        rec.expires_at_ms,
    );
    vk.verify_strict(&msg, &sig)
        .map_err(|_| KeyPackageVerifyError::BadSig)?;

    // 4. Lower bound: not already expired.
    if rec.expires_at_ms <= now_ms {
        return Err(KeyPackageVerifyError::Expired);
    }

    // 5. Upper bound: not too far in the future. A KP minted with
    //    `expires_at_ms = now + KEYPACKAGE_LIFETIME_MS` is the
    //    canonical case; we allow MAX_KP_SKEW_MS slop on top of that
    //    so a publisher with a slightly-fast clock isn't rejected.
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

// ---------------------------------------------------------------------------
// Helper: verify the publisher's outer signature on a batch
// ---------------------------------------------------------------------------

/// Verify a publisher's outer signature on a batch using the supplied
/// `domain_helper` (so Publish and Refill share this code despite
/// using different domain-tag transcripts).
///
/// `now_ms` is needed for the timestamp-skew check.
fn verify_outer_sig(
    publisher_ipk: &[u8; 32], outer_sig: &[u8; 64], records: &[KeyPackageRecord],
    timestamp: u64, is_refill: bool, now_ms: u64,
) -> bool {
    // Skew check first — cheap, runs before any crypto.
    let skew = now_ms.abs_diff(timestamp);
    if skew > MAX_KP_SKEW_MS {
        return false;
    }

    let Ok(vk) = VerifyingKey::from_bytes(publisher_ipk) else {
        return false;
    };

    let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
    let count = records.len() as u32;
    let msg = if is_refill {
        kp_refill_signing_input(MLS_WIRE_VERSION, publisher_ipk, &digest, count, timestamp)
    } else {
        kp_publish_signing_input(MLS_WIRE_VERSION, publisher_ipk, &digest, count, timestamp)
    };
    let sig = Signature::from_bytes(outer_sig);
    vk.verify_strict(&msg, &sig).is_ok()
}

// ---------------------------------------------------------------------------
// Storage primitives — visible to tests and to the handlers
// ---------------------------------------------------------------------------

/// Insert a record into the stash. Idempotent on byte-identical
/// re-publishes; returns `Err(InsertError::StaticFieldsConflict)` if a
/// record with the same `(ipk, kp_ref)` already exists with
/// **different** value bytes (forgery detection).
///
/// All silent no-op paths surface as `InsertError::Storage`; the caller
/// maps this to [`KeyPackagePublishOutcome::BadSig`] (the closest
/// existing wire outcome that signals "this didn't take") and logs the
/// underlying cause. An encode failure or a fjall
/// write error must not return `Ok(())` and let the publish handler
/// report `Stored` — that would be silent data-loss-as-success.
///
/// Caller is expected to have already verified the record via
/// [`verify_record`].
fn insert_record(dht: &Dht, rec: &KeyPackageRecord) -> Result<(), InsertError> {
    let key = storage_key(&rec.ipk.0, &rec.kp_ref.0)
        .ok_or_else(|| InsertError::Storage("malformed kp_ref length".into()))?;

    let bytes = rec
        .ser()
        .map_err(|e| InsertError::Storage(format!("serialize: {e:?}")))?;

    // If a record already exists for this `(ipk, kp_ref)`, demand
    // byte-identity. The owner-sig transcript binds `BLAKE3(kp_bytes)`,
    // so different `kp_bytes` for the same `kp_ref` would have failed
    // `verify_record` first — but a malicious replica could still try
    // to slip a stale record past us. The byte-identity check is
    // defense-in-depth.
    match dht.store.keypackage.get(key) {
        Ok(Some(existing)) => {
            if existing[..] != bytes[..] {
                return Err(InsertError::StaticFieldsConflict);
            }
            // Byte-identical: idempotent no-op (we still re-write below
            // for fsync-driven freshness — same policy as
            // store.rs::store_record).
        }
        Ok(None) => {}
        Err(e) => {
            return Err(InsertError::Storage(format!("get: {e}")));
        }
    }

    dht.store
        .put_sync(&dht.store.keypackage, key, &bytes)
        .map_err(|e| InsertError::Storage(format!("put: {e}")))?;
    Ok(())
}

/// Typed error from `insert_record`. Allows the publish handler to
/// distinguish a forgery-detection failure (which has its own outcome
/// `StaticFieldsConflict`) from a generic storage failure (which
/// surfaces as `BadSig` plus a relay-side log).
#[derive(Debug)]
enum InsertError {
    /// Republish for an existing `(ipk, kp_ref)` carried different
    /// `kp_bytes`.
    StaticFieldsConflict,
    /// Underlying storage failure (fjall I/O, encode error, etc.).
    Storage(String),
}

/// Truncated-IPK formatter for log lines. Returns the first 8 hex
/// chars (4 bytes) of the IPK so logs don't leak the full conversation
/// graph from a captured device's log buffer.
fn fmt_ipk(ipk: &[u8; 32]) -> String {
    hex::encode(&ipk[..4])
}

/// Truncated-bytes formatter for log lines. Same idea as [`fmt_ipk`]
/// but accepts a byte slice (used for kp_ref, group_id, dispatch_id).
fn fmt_short(bytes: &[u8]) -> String {
    let n = bytes.len().min(4);
    hex::encode(&bytes[..n])
}

/// Walk all records in this stash. Returns `(storage_key, record)`
/// pairs in iterator order (which is `kp_ref`-byte order — opaque to
/// callers, sufficient for "pick the first non-expired" semantics).
fn iterate_stash(dht: &Dht, ipk: &[u8; 32]) -> Vec<([u8; STORAGE_KEY_LEN], KeyPackageRecord)> {
    let mut out = Vec::new();
    let prefix = stash_prefix(ipk);
    for guard in dht.store.keypackage.prefix(prefix) {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => break,
        };
        if key_bytes.len() != STORAGE_KEY_LEN {
            continue;
        }
        let mut k = [0u8; STORAGE_KEY_LEN];
        k.copy_from_slice(&key_bytes);
        let Ok(rec) = KeyPackageRecord::deser(&value) else {
            continue;
        };
        out.push((k, rec));
    }
    out
}

// ---------------------------------------------------------------------------
// Static-hash helper for KeyPackageFetch responses
// ---------------------------------------------------------------------------

/// Compute the "static hash" surface from a record. Defined as
/// `BLAKE3(target_ipk || credential_ipk || credential_signing_key_bytes)`
/// — fields the requester can cross-check across K replicas to detect
/// a malicious home substituting a forged KP.
///
/// Because the owner-sig transcript folds in `BLAKE3(kp_bytes)`, a
/// replica that stored tampered `kp_bytes` for an existing
/// `(ipk, kp_ref)` would have failed publish-time `verify_record`
/// first — so the cross-replica check is defense-in-depth rather than
/// the primary forgery gate. We extend the static hash to also bind
/// `BLAKE3(kp_bytes)` directly: any byte-different `kp_bytes` between
/// replicas surfaces as a different `static_hash`.
///
/// We don't parse the openmls TLS-encoded `KeyPackage` to extract the
/// inner credential's signing-key bytes — that requires the openmls
/// dependency which the relay deliberately avoids. The
/// `kp_bytes`-digest binding is the cheapest stand-in: the inner
/// credential signing-key bytes are a *subset* of `kp_bytes`, so a
/// substitution that changes them necessarily changes the digest.
fn compute_static_hash(rec: &KeyPackageRecord) -> [u8; 32] {
    // BLAKE3(target_ipk || kp_ref || BLAKE3(kp_bytes)). Same hashing
    // pattern as `stash_prefix` / sync/merkle.rs — use `NodeId::new`
    // (BLAKE3 wrapper) so the relay doesn't need a direct `blake3`
    // dep; nest one BLAKE3 over `kp_bytes` to keep the inner-body
    // binding cheap on large KPs.
    let kp_bytes_digest = *NodeId::new(&rec.kp_bytes.0).as_bytes();
    let mut buf = Vec::with_capacity(32 + rec.kp_ref.0.len() + 32);
    buf.extend_from_slice(&rec.ipk.0);
    buf.extend_from_slice(&rec.kp_ref.0);
    buf.extend_from_slice(&kp_bytes_digest);
    *NodeId::new(&buf).as_bytes()
}

// ---------------------------------------------------------------------------
// Public API — handlers
// ---------------------------------------------------------------------------

/// Home-side handler for [`DhtRequest::KeyPackagePublish`].
///
/// Validation ladder:
/// 1. `req.records.len() <= KP_STASH_TARGET` — bound check first.
/// 2. Outer `sig` verifies under `req.ipk` over the canonical
///    transcript (incorporates the BLAKE3 records-digest).
/// 3. Self is in K-closest for `stash_prefix(ipk)`.
/// 4. For each record:
///    - per-record `owner_sig` verifies under `req.ipk` over
///      [`kp_record_signing_input`].
///    - `expires_at_ms > now_ms`.
///    - record's `ipk` field equals `req.ipk` (no smuggling).
/// 5. Insert into fjall. A static-fields conflict surfaces as
///    `StaticFieldsConflict` outcome.
///
/// `_authenticated_peer_id` is currently unused — KeyPackagePublish
/// is owner-authored, and the relay-to-relay `peer/1` connection's
/// authenticated peer id is the *relay forwarding* the publish, not
/// the owner. The owner authentication lives in `req.sig` (verified
/// under `req.ipk`). A relay-binding check may be added later if the
/// flow needs it; the parameter is reserved for that future tightening.
pub(crate) fn handle_keypackage_publish(
    dht: &Arc<Dht>, req: KeyPackagePublishReq, _authenticated_peer_id: NodeId, now_ms: u64,
) -> KeyPackagePublishOutcome {
    use common::proto::mls_wire::KP_STASH_TARGET;

    // 1. Bound check.
    if req.records.len() > KP_STASH_TARGET {
        return KeyPackagePublishOutcome::TooMany;
    }
    if req.records.is_empty() {
        // Empty publish is a no-op; treat as Stored for idempotency.
        return KeyPackagePublishOutcome::Stored;
    }

    // 2. Outer sig.
    if !verify_outer_sig(
        &req.ipk.0,
        &req.sig.0,
        &req.records,
        req.timestamp,
        false, // is_refill
        now_ms,
    ) {
        return KeyPackagePublishOutcome::BadSig;
    }

    // 3. Ownership.
    if !self_is_owner_for_stash(dht, &req.ipk.0) {
        return KeyPackagePublishOutcome::NotOwner;
    }

    // 4. Per-record verify + cross-check `ipk` field.
    for rec in &req.records {
        if rec.ipk.0 != req.ipk.0 {
            // Smuggling attempt — a record whose `ipk` claims someone
            // else under a publisher who also claims to be the owner.
            return KeyPackagePublishOutcome::BadSig;
        }
        match verify_record(rec, now_ms) {
            Ok(()) => {}
            Err(KeyPackageVerifyError::Expired) => return KeyPackagePublishOutcome::Expired,
            Err(_) => return KeyPackagePublishOutcome::BadSig,
        }
    }

    // 5. Insert.
    for rec in &req.records {
        match insert_record(dht, rec) {
            Ok(()) => {}
            Err(InsertError::StaticFieldsConflict) => {
                common::warn!(
                    "MLS publish: static-fields conflict for ipk={} kp_ref={}",
                    fmt_ipk(&rec.ipk.0),
                    fmt_short(&rec.kp_ref.0)
                );
                return KeyPackagePublishOutcome::StaticFieldsConflict;
            }
            Err(InsertError::Storage(e)) => {
                common::warn!(
                    "MLS publish: storage failure for ipk={} kp_ref={}: {e}",
                    fmt_ipk(&rec.ipk.0),
                    fmt_short(&rec.kp_ref.0)
                );
                return KeyPackagePublishOutcome::BadSig;
            }
        }
    }

    common::debug!(
        "MLS publish: stored {} record(s) for ipk={}",
        req.records.len(),
        fmt_ipk(&req.ipk.0)
    );
    KeyPackagePublishOutcome::Stored
}

/// Home-side handler for [`DhtRequest::KeyPackageRefill`].
///
/// Identical validation ladder to [`handle_keypackage_publish`]
/// modulo the outer-sig domain — refill uses [`KP_REFILL_DOMAIN`] so
/// a captured Publish sig cannot be replayed as a Refill.
pub(crate) fn handle_keypackage_refill(
    dht: &Arc<Dht>, req: KeyPackageRefillReq, _authenticated_peer_id: NodeId, now_ms: u64,
) -> KeyPackageRefillOutcome {
    use common::proto::mls_wire::KP_STASH_TARGET;

    if req.records.len() > KP_STASH_TARGET {
        return KeyPackageRefillOutcome::TooMany;
    }
    if req.records.is_empty() {
        return KeyPackageRefillOutcome::Appended;
    }

    if !verify_outer_sig(
        &req.ipk.0,
        &req.sig.0,
        &req.records,
        req.timestamp,
        true, // is_refill
        now_ms,
    ) {
        return KeyPackageRefillOutcome::BadSig;
    }

    if !self_is_owner_for_stash(dht, &req.ipk.0) {
        return KeyPackageRefillOutcome::NotOwner;
    }

    for rec in &req.records {
        if rec.ipk.0 != req.ipk.0 {
            return KeyPackageRefillOutcome::BadSig;
        }
        match verify_record(rec, now_ms) {
            Ok(()) => {}
            Err(KeyPackageVerifyError::Expired) => return KeyPackageRefillOutcome::Expired,
            Err(_) => return KeyPackageRefillOutcome::BadSig,
        }
    }

    for rec in &req.records {
        match insert_record(dht, rec) {
            Ok(()) => {}
            Err(InsertError::StaticFieldsConflict) => {
                common::warn!(
                    "MLS refill: static-fields conflict for ipk={} kp_ref={}",
                    fmt_ipk(&rec.ipk.0),
                    fmt_short(&rec.kp_ref.0)
                );
                return KeyPackageRefillOutcome::StaticFieldsConflict;
            }
            Err(InsertError::Storage(e)) => {
                common::warn!(
                    "MLS refill: storage failure for ipk={} kp_ref={}: {e}",
                    fmt_ipk(&rec.ipk.0),
                    fmt_short(&rec.kp_ref.0)
                );
                return KeyPackageRefillOutcome::BadSig;
            }
        }
    }

    common::debug!(
        "MLS refill: appended {} record(s) for ipk={}",
        req.records.len(),
        fmt_ipk(&req.ipk.0)
    );
    KeyPackageRefillOutcome::Appended
}

/// Home-side handler for [`DhtRequest::KeyPackageFetch`].
///
/// Validation ladder:
/// 1. `req.requester_relay_id == authenticated_peer_id` — defends
///    against cross-relay replay (mirrors the QueueFetch handler).
/// 2. Skew check on `req.timestamp`.
/// 3. Self is in K-closest for `stash_prefix(target_ipk)`.
/// 4. Per-pair rate-limit check on `(target_ipk, requester_relay_id)`
///    via [`KpFetchLimiters`].
/// 5. Pop the first non-expired record from the stash (FIFO over
///    iterator order, which is opaque-deterministic by `kp_ref` byte
///    order). Delete it from fjall. Return `Found(record,
///    remaining, static_hash)`.
///
/// Empty stash → `NoStash`. All paths are sync; we touch fjall
/// directly without any `await`.
pub(crate) fn handle_keypackage_fetch(
    dht: &Arc<Dht>, req: KeyPackageFetchReq, authenticated_peer_id: NodeId, now_ms: u64,
) -> KeyPackageFetchOutcome {
    // 1. Requester binding (cross-relay replay defence).
    if req.requester_relay_id != authenticated_peer_id {
        // Treat as rate-limited so a probing relay doesn't get a
        // distinct error code that leaks "this user has a stash".
        // Same defensive shape as `QueueFetch`'s redirected-requester
        // path (which returns empty + exhausted).
        common::warn!(
            "MLS kp_fetch: requester binding mismatch for target_ipk={}",
            fmt_ipk(&req.target_ipk.0)
        );
        return KeyPackageFetchOutcome::RateLimited;
    }

    // 2. Skew check.
    let skew = now_ms.abs_diff(req.timestamp);
    if skew > MAX_KP_SKEW_MS {
        // Same response shape as a rate-limit (no need for a
        // separate "ClockSkew" outcome on this RPC — the requester
        // can re-sync their clock and retry).
        common::warn!(
            "MLS kp_fetch: skew {}ms > max {}ms for target_ipk={}",
            skew,
            MAX_KP_SKEW_MS,
            fmt_ipk(&req.target_ipk.0)
        );
        return KeyPackageFetchOutcome::RateLimited;
    }

    // 3. Ownership.
    if !self_is_owner_for_stash(dht, &req.target_ipk.0) {
        return KeyPackageFetchOutcome::NotOwner;
    }

    // 4. Per-pair rate-limit.
    if dht
        .kp_fetch_limiters
        .check(&req.target_ipk.0, &req.requester_relay_id)
        .is_err()
    {
        common::debug!(
            "MLS kp_fetch: per-pair rate-limit hit for target_ipk={}",
            fmt_ipk(&req.target_ipk.0)
        );
        return KeyPackageFetchOutcome::RateLimited;
    }

    // 5. Pop the first non-expired record. We collect-then-pop so we
    //    can return both the popped record and a `remaining` count.
    let mut stash = iterate_stash(dht, &req.target_ipk.0);

    // Filter expired records (silently — the publisher's responsibility
    // to refill before lifetime elapses).
    let mut popped: Option<(usize, KeyPackageRecord)> = None;
    let mut to_evict: Vec<[u8; STORAGE_KEY_LEN]> = Vec::new();
    for (idx, (key, rec)) in stash.iter().enumerate() {
        if rec.expires_at_ms <= now_ms {
            to_evict.push(*key);
            continue;
        }
        popped = Some((idx, rec.clone()));
        break;
    }

    // Best-effort opportunistic eviction of expired records.
    for k in &to_evict {
        let _ = dht.store.keypackage.remove(k);
    }

    let Some((popped_idx, popped_record)) = popped else {
        return KeyPackageFetchOutcome::NoStash;
    };

    // Strict one-shot: delete the popped record before returning so
    // a duplicate fetch can't re-vend it.
    let popped_key = stash[popped_idx].0;
    let _ = dht.store.keypackage.remove(popped_key);

    // Rebuild "remaining" without re-iterating: count non-expired
    // records still on disk after the pop. We've already evicted
    // expired ones above, so this is `stash.len() - to_evict.len() - 1`.
    let evicted_set: std::collections::HashSet<[u8; STORAGE_KEY_LEN]> =
        to_evict.into_iter().collect();
    stash.retain(|(k, _)| !evicted_set.contains(k) && *k != popped_key);
    let remaining = stash.len() as u32;

    let static_hash = compute_static_hash(&popped_record);
    KeyPackageFetchOutcome::Found(KeyPackageFetchFound {
        record: popped_record,
        remaining,
        static_hash: static_hash.into(),
    })
}

/// Map a [`KeyPackagePublishOutcome`] to the wire response shape.
pub(crate) fn wrap_publish_outcome(
    outcome: KeyPackagePublishOutcome,
) -> KeyPackagePublishResp {
    KeyPackagePublishResp { outcome }
}

/// Map a [`KeyPackageRefillOutcome`] to the wire response shape.
pub(crate) fn wrap_refill_outcome(
    outcome: KeyPackageRefillOutcome,
) -> KeyPackageRefillResp {
    KeyPackageRefillResp { outcome }
}

/// Map a [`KeyPackageFetchOutcome`] to the wire response shape.
pub(crate) fn wrap_fetch_outcome(outcome: KeyPackageFetchOutcome) -> KeyPackageFetchResp {
    KeyPackageFetchResp { outcome }
}

// ---------------------------------------------------------------------------
// CloseReason mapping helpers
// ---------------------------------------------------------------------------

/// Map a `KeyPackagePublishOutcome` to the `CloseReason` that
/// represents a *hard* protocol violation (signature/static-fields/
/// length). Soft outcomes (`NotOwner`, `RateLimited`, `Stored`) are
/// not protocol violations and surface only in the response body —
/// the per-stream dispatcher does not close the connection on them.
///
/// Used by tests and (in future hardening passes) by the dispatcher
/// to optionally close a misbehaving peer's connection on hard
/// failures. These are currently surfaced solely via the response body
/// (consistent with the existing `Forward`/`QueueFetch` conventions
/// — soft-reject the request without dropping the connection so a
/// briefly-misconfigured peer doesn't cascade-fail).
pub(crate) fn close_reason_for_publish(
    outcome: KeyPackagePublishOutcome,
) -> Option<common::quic::CloseReason> {
    use common::quic::CloseReason;
    match outcome {
        KeyPackagePublishOutcome::BadSig
        | KeyPackagePublishOutcome::TooMany
        | KeyPackagePublishOutcome::StaticFieldsConflict => Some(CloseReason::KeyPackageMalformed),
        KeyPackagePublishOutcome::Expired => Some(CloseReason::KeyPackageExpired),
        KeyPackagePublishOutcome::RateLimited => Some(CloseReason::KeyPackageRateLimited),
        KeyPackagePublishOutcome::NotOwner | KeyPackagePublishOutcome::Stored => None,
    }
}

/// Mirror of [`close_reason_for_publish`] for fetch outcomes.
pub(crate) fn close_reason_for_fetch(
    outcome: &KeyPackageFetchOutcome,
) -> Option<common::quic::CloseReason> {
    use common::quic::CloseReason;
    match outcome {
        KeyPackageFetchOutcome::RateLimited => Some(CloseReason::KeyPackageRateLimited),
        KeyPackageFetchOutcome::NotOwner
        | KeyPackageFetchOutcome::Found(_)
        | KeyPackageFetchOutcome::NoStash => None,
    }
}

/// Mirror of [`close_reason_for_publish`] for refill outcomes.
pub(crate) fn close_reason_for_refill(
    outcome: KeyPackageRefillOutcome,
) -> Option<common::quic::CloseReason> {
    use common::quic::CloseReason;
    match outcome {
        KeyPackageRefillOutcome::BadSig
        | KeyPackageRefillOutcome::TooMany
        | KeyPackageRefillOutcome::StaticFieldsConflict => Some(CloseReason::KeyPackageMalformed),
        KeyPackageRefillOutcome::Expired => Some(CloseReason::KeyPackageExpired),
        KeyPackageRefillOutcome::RateLimited => Some(CloseReason::KeyPackageRateLimited),
        KeyPackageRefillOutcome::NotOwner | KeyPackageRefillOutcome::Appended => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    
    use common::proto::mls_wire::KP_STASH_TARGET;
    use common::proto::mls_wire::MLS_WIRE_VERSION;
    use common::proto::mls_wire::kp_publish_records_digest;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;

    /// Deterministic-distinct seed counter — same idiom as
    /// `store::tests::fresh_signing_key` so test fixtures don't
    /// require RNG.
    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        seed[16] = ((n >> 8) & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    /// Build a `Dht` with the new `cf_dht_keypackage` CF registered.
    /// Mirrors `store::tests::fresh_dht`.
    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("promtuz-mls-kp-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
    }

    /// Build a self-consistent `KeyPackageRecord`. `kp_ref` is a
    /// 32-byte synthetic digest; for production use, this would be
    /// `SHA-256(tls_encode(kp_bytes))`.
    fn build_record(
        owner: &SigningKey, kp_ref: [u8; 32], kp_bytes: Vec<u8>, expires_at_ms: u64,
    ) -> KeyPackageRecord {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &kp_ref,
            &kp_bytes,
            expires_at_ms,
        );
        let sig = owner.sign(&msg);
        KeyPackageRecord {
            ipk: ipk.into(),
            kp_ref: kp_ref.to_vec().into(),
            kp_bytes: kp_bytes.into(),
            expires_at_ms,
            owner_sig: sig.to_bytes().into(),
        }
    }

    /// Build a fully-signed `KeyPackagePublishReq` carrying
    /// `records`, signed by `owner`.
    fn build_publish(
        owner: &SigningKey, records: Vec<KeyPackageRecord>, timestamp: u64,
    ) -> KeyPackagePublishReq {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, &records);
        let msg = kp_publish_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &digest,
            records.len() as u32,
            timestamp,
        );
        let sig = owner.sign(&msg);
        KeyPackagePublishReq {
            ipk: ipk.into(),
            records,
            timestamp,
            sig: sig.to_bytes().into(),
        }
    }

    /// Real wall-clock now in ms. Tests that verify timestamp-skew
    /// against the handler's `now_ms` argument can pin a deterministic
    /// `now` instead — most tests do.
    fn fresh_now() -> u64 {
        1_700_000_000_000
    }

    // ---------------------------------------------------------------
    // 1. Publish + Fetch round-trip (single record)
    // ---------------------------------------------------------------

    #[test]
    fn publish_then_fetch_round_trip_single_record() {
        let owner = fresh_signing_key();
        // Self_id need not match the owner — sparse-table policy
        // is permissive, so we'll be admitted as K-closest by
        // default.
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();

        let kp_ref = [0xAA; 32];
        let rec = build_record(&owner, kp_ref, b"kp-bytes".to_vec(), now + 60_000);
        let pub_req = build_publish(&owner, vec![rec.clone()], now);
        let auth_peer = NodeId::new([0xBB; 32]);

        let outcome = handle_keypackage_publish(&dht, pub_req, auth_peer, now);
        assert_eq!(outcome, KeyPackagePublishOutcome::Stored);

        // Fetch — requester must equal authenticated peer.
        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let fetch_req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: now,
        };
        let outcome = handle_keypackage_fetch(&dht, fetch_req, auth_peer, now);
        match outcome {
            KeyPackageFetchOutcome::Found(found) => {
                assert_eq!(found.record, rec);
                assert_eq!(found.remaining, 0, "stash empty after popping the only KP");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // 2. Publish + Fetch round-trip (multiple records)
    // ---------------------------------------------------------------

    #[test]
    fn publish_then_fetch_with_multiple_records() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Three records.
        let recs: Vec<KeyPackageRecord> = (0..3u8)
            .map(|i| {
                let mut kp_ref = [0u8; 32];
                kp_ref[0] = i;
                build_record(&owner, kp_ref, vec![i], now + 60_000)
            })
            .collect();
        let pub_req = build_publish(&owner, recs.clone(), now);
        assert_eq!(
            handle_keypackage_publish(&dht, pub_req, auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let fetch_req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: now,
        };
        // First fetch consumes one; remaining = 2.
        match handle_keypackage_fetch(&dht, fetch_req.clone(), auth_peer, now) {
            KeyPackageFetchOutcome::Found(f) => assert_eq!(f.remaining, 2),
            other => panic!("first fetch: {other:?}"),
        }
        // Second consumes another; remaining = 1.
        match handle_keypackage_fetch(&dht, fetch_req.clone(), auth_peer, now) {
            KeyPackageFetchOutcome::Found(f) => assert_eq!(f.remaining, 1),
            other => panic!("second fetch: {other:?}"),
        }
        // Third leaves stash empty; remaining = 0.
        match handle_keypackage_fetch(&dht, fetch_req.clone(), auth_peer, now) {
            KeyPackageFetchOutcome::Found(f) => assert_eq!(f.remaining, 0),
            other => panic!("third fetch: {other:?}"),
        }
        // Fourth: NoStash.
        assert!(matches!(
            handle_keypackage_fetch(&dht, fetch_req, auth_peer, now),
            KeyPackageFetchOutcome::NoStash
        ));
    }

    // ---------------------------------------------------------------
    // 3. Fetch returns NoStash when ipk has nothing
    // ---------------------------------------------------------------

    #[test]
    fn fetch_returns_no_stash_for_unknown_ipk() {
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let unknown_ipk = [0x99; 32];
        let req = KeyPackageFetchReq {
            target_ipk: unknown_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: now,
        };
        assert!(matches!(
            handle_keypackage_fetch(&dht, req, auth_peer, now),
            KeyPackageFetchOutcome::NoStash
        ));
    }

    // ---------------------------------------------------------------
    // 4. Fetch is one-time-use (each fetch returns a distinct KP)
    // ---------------------------------------------------------------

    #[test]
    fn fetch_consumes_each_record_exactly_once() {
        // Consumed KPs cannot be re-vended — strict one-shot semantics.
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let recs: Vec<KeyPackageRecord> = (0..3u8)
            .map(|i| {
                let mut kp_ref = [0u8; 32];
                kp_ref[0] = 100 + i; // distinguishable
                build_record(&owner, kp_ref, vec![i], now + 60_000)
            })
            .collect();
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, recs.clone(), now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let fetch_req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: now,
        };

        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for _ in 0..3 {
            match handle_keypackage_fetch(&dht, fetch_req.clone(), auth_peer, now) {
                KeyPackageFetchOutcome::Found(f) => {
                    let inserted = seen.insert(f.record.kp_ref.0.clone());
                    assert!(inserted, "each fetch must return a distinct kp_ref");
                }
                other => panic!("expected Found, got {other:?}"),
            }
        }
        assert_eq!(seen.len(), 3, "all three KPs consumed exactly once");
    }

    // ---------------------------------------------------------------
    // 5. Expired record on store → rejected; expired on fetch →
    //    silently filtered.
    // ---------------------------------------------------------------

    #[test]
    fn publish_with_expired_record_is_rejected() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // expires_at_ms == now → already expired (we use `<=` in the
        // verifier).
        let rec = build_record(&owner, [0xCC; 32], b"x".to_vec(), now);
        let req = build_publish(&owner, vec![rec], now);
        assert_eq!(
            handle_keypackage_publish(&dht, req, auth_peer, now),
            KeyPackagePublishOutcome::Expired
        );
    }

    #[test]
    fn fetch_silently_filters_expired_records() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let auth_peer = NodeId::new([0xBB; 32]);

        // Publish two records: one short-lived, one long-lived. We
        // publish at `t0` so both are in-window, then fetch at `t1 >
        // short_expires` so the short one is silently filtered.
        let t0 = fresh_now();
        let short = build_record(&owner, [0x01; 32], b"short".to_vec(), t0 + 1_000);
        let long = build_record(&owner, [0x02; 32], b"long".to_vec(), t0 + 60_000);
        let pub_req = build_publish(&owner, vec![short.clone(), long.clone()], t0);
        assert_eq!(
            handle_keypackage_publish(&dht, pub_req, auth_peer, t0),
            KeyPackagePublishOutcome::Stored
        );

        // Skip past short.expires_at_ms; long is still alive.
        let t1 = t0 + 5_000;
        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let fetch_req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: t1,
        };
        match handle_keypackage_fetch(&dht, fetch_req, auth_peer, t1) {
            KeyPackageFetchOutcome::Found(f) => {
                assert_eq!(f.record.kp_ref.0, long.kp_ref.0);
                assert_eq!(f.remaining, 0, "expired short was filtered");
            }
            other => panic!("expected Found(long), got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // 6. Owner-sig verification: bad sig → store rejected
    // ---------------------------------------------------------------

    #[test]
    fn publish_with_tampered_owner_sig_is_rejected() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let mut rec = build_record(&owner, [0xAA; 32], b"x".to_vec(), now + 60_000);
        // Tamper the per-record owner_sig.
        rec.owner_sig.0[0] ^= 0xFF;
        let req = build_publish(&owner, vec![rec], now);
        assert_eq!(
            handle_keypackage_publish(&dht, req, auth_peer, now),
            KeyPackagePublishOutcome::BadSig
        );
    }

    #[test]
    fn publish_with_tampered_outer_sig_is_rejected() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let rec = build_record(&owner, [0xAA; 32], b"x".to_vec(), now + 60_000);
        let mut req = build_publish(&owner, vec![rec], now);
        req.sig.0[0] ^= 0xFF;
        assert_eq!(
            handle_keypackage_publish(&dht, req, auth_peer, now),
            KeyPackagePublishOutcome::BadSig
        );
    }

    // ---------------------------------------------------------------
    // 7. Static-fields conflict: republish with different bytes for
    //    same (ipk, kp_ref) → rejected
    // ---------------------------------------------------------------

    #[test]
    fn republish_with_different_kp_bytes_is_static_fields_conflict() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let kp_ref = [0xDD; 32];
        let rec_a = build_record(&owner, kp_ref, b"original-bytes".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_a], now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        // Same (ipk, kp_ref) but different kp_bytes — forgery attempt.
        let rec_b = build_record(&owner, kp_ref, b"FORGED-bytes".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_b], now), auth_peer, now),
            KeyPackagePublishOutcome::StaticFieldsConflict
        );
    }

    /// Anti-pinning rotation: clients periodically rotate their stash
    /// even with no consumption. A Publish for a fresh batch with new
    /// `kp_ref`s does *not* lose the old records — they remain
    /// consumable until natural expiry. This test pins that property.
    #[test]
    fn fresh_publish_does_not_evict_old_records() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Publish batch A.
        let rec_a = build_record(&owner, [0xA1; 32], b"alpha".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_a.clone()], now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        // Publish batch B (different kp_refs). Spec says: additive,
        // not replacing. The publisher just pushed a fresh "rotation"
        // batch; the old in-lifetime KP_a is still consumable.
        let rec_b = build_record(&owner, [0xB1; 32], b"beta".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_b.clone()], now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        // Both records survive in the stash.
        let stash = iterate_stash(&dht, &rec_a.ipk.0);
        assert_eq!(stash.len(), 2, "rotation must not evict in-lifetime old records");
        let kp_refs: std::collections::HashSet<Vec<u8>> =
            stash.iter().map(|(_, r)| r.kp_ref.0.clone()).collect();
        assert!(kp_refs.contains(&rec_a.kp_ref.0));
        assert!(kp_refs.contains(&rec_b.kp_ref.0));
    }

    // ---------------------------------------------------------------
    // 8. Rate limiting
    // ---------------------------------------------------------------

    #[test]
    fn fetch_rate_limit_trips_per_pair() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Pre-populate the stash with enough records that the cap is
        // not the limiting factor in this test.
        let recs: Vec<KeyPackageRecord> = (0..KP_STASH_TARGET as u8)
            .map(|i| {
                let mut kp_ref = [0u8; 32];
                kp_ref[0] = i;
                build_record(&owner, kp_ref, vec![i], now + 60_000)
            })
            .collect();
        // Need to publish in batches to stay under the request-size
        // cap. For this test, just push the maximum batch in one go.
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, recs, now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let fetch_req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: auth_peer,
            timestamp: now,
        };

        // Hammer fetches for the same `(target_ipk, requester)` pair.
        // The per-pair quota is 60/hour with burst 60; after the
        // burst is drained the limiter denies. We count denials in a
        // bounded loop so the test stays under 2 s.
        let mut denied = 0usize;
        for _ in 0..(MAX_KP_FETCH_PER_HOUR as usize + 5) {
            match handle_keypackage_fetch(&dht, fetch_req.clone(), auth_peer, now) {
                KeyPackageFetchOutcome::Found(_) | KeyPackageFetchOutcome::NoStash => {}
                KeyPackageFetchOutcome::RateLimited => denied += 1,
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert!(denied > 0, "rate limit must trip after the burst is exhausted");
    }

    #[test]
    fn fetch_rate_limit_does_not_share_quota_across_requesters() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();

        // Two distinct requester relays.
        let req_a = NodeId::new([0xAA; 32]);
        let req_b = NodeId::new([0xBB; 32]);

        // Pre-populate enough stash for two parallel fetches.
        let recs: Vec<KeyPackageRecord> = (0..2u8)
            .map(|i| {
                let mut kp_ref = [0u8; 32];
                kp_ref[0] = i;
                build_record(&owner, kp_ref, vec![i], now + 60_000)
            })
            .collect();
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, recs, now), req_a, now),
            KeyPackagePublishOutcome::Stored
        );

        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        // Drain req_a's burst.
        let fetch_a = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: req_a,
            timestamp: now,
        };
        for _ in 0..(MAX_KP_FETCH_PER_HOUR as usize + 5) {
            let _ = handle_keypackage_fetch(&dht, fetch_a.clone(), req_a, now);
        }

        // req_b's first fetch should still be allowed (different
        // requester => different per-pair key). The stash may be
        // empty by now, but the quota check must not be the
        // gate-keeping reason — so the outcome must NOT be
        // `RateLimited`.
        let fetch_b = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: req_b,
            timestamp: now,
        };
        let outcome = handle_keypackage_fetch(&dht, fetch_b, req_b, now);
        assert!(
            !matches!(outcome, KeyPackageFetchOutcome::RateLimited),
            "different requester must not share quota; got {outcome:?}"
        );
    }

    #[test]
    fn fetch_rate_limit_does_not_share_quota_across_targets() {
        // Two distinct target IPKs at the same requester. Draining
        // target_a's burst must not affect target_b's quota.
        let owner_a = fresh_signing_key();
        let owner_b = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let req = NodeId::new([0xCC; 32]);

        // Each owner publishes one KP.
        let rec_a = build_record(&owner_a, [0xA1; 32], b"a".to_vec(), now + 60_000);
        let rec_b = build_record(&owner_b, [0xB1; 32], b"b".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner_a, vec![rec_a], now), req, now),
            KeyPackagePublishOutcome::Stored
        );
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner_b, vec![rec_b], now), req, now),
            KeyPackagePublishOutcome::Stored
        );

        let ipk_a: [u8; 32] = owner_a.verifying_key().to_bytes();
        let ipk_b: [u8; 32] = owner_b.verifying_key().to_bytes();

        // Drain target_a's burst.
        let fetch_a = KeyPackageFetchReq {
            target_ipk: ipk_a.into(),
            requester_relay_id: req,
            timestamp: now,
        };
        for _ in 0..(MAX_KP_FETCH_PER_HOUR as usize + 5) {
            let _ = handle_keypackage_fetch(&dht, fetch_a.clone(), req, now);
        }

        // target_b should not be rate-limited.
        let fetch_b = KeyPackageFetchReq {
            target_ipk: ipk_b.into(),
            requester_relay_id: req,
            timestamp: now,
        };
        let outcome = handle_keypackage_fetch(&dht, fetch_b, req, now);
        assert!(
            !matches!(outcome, KeyPackageFetchOutcome::RateLimited),
            "different target must not share quota; got {outcome:?}"
        );
    }

    // ---------------------------------------------------------------
    // 9. CloseReason mapping
    // ---------------------------------------------------------------

    #[test]
    fn close_reason_mapping_for_publish_outcomes() {
        use common::quic::CloseReason;
        // Hard-fail outcomes map to specific CloseReasons.
        assert!(matches!(
            close_reason_for_publish(KeyPackagePublishOutcome::BadSig),
            Some(CloseReason::KeyPackageMalformed)
        ));
        assert!(matches!(
            close_reason_for_publish(KeyPackagePublishOutcome::TooMany),
            Some(CloseReason::KeyPackageMalformed)
        ));
        assert!(matches!(
            close_reason_for_publish(KeyPackagePublishOutcome::StaticFieldsConflict),
            Some(CloseReason::KeyPackageMalformed)
        ));
        assert!(matches!(
            close_reason_for_publish(KeyPackagePublishOutcome::Expired),
            Some(CloseReason::KeyPackageExpired)
        ));
        assert!(matches!(
            close_reason_for_publish(KeyPackagePublishOutcome::RateLimited),
            Some(CloseReason::KeyPackageRateLimited)
        ));
        // Soft outcomes don't trigger a close.
        assert!(close_reason_for_publish(KeyPackagePublishOutcome::Stored).is_none());
        assert!(close_reason_for_publish(KeyPackagePublishOutcome::NotOwner).is_none());
    }

    #[test]
    fn close_reason_mapping_for_fetch_outcomes() {
        use common::quic::CloseReason;
        assert!(matches!(
            close_reason_for_fetch(&KeyPackageFetchOutcome::RateLimited),
            Some(CloseReason::KeyPackageRateLimited)
        ));
        assert!(close_reason_for_fetch(&KeyPackageFetchOutcome::NoStash).is_none());
        assert!(close_reason_for_fetch(&KeyPackageFetchOutcome::NotOwner).is_none());
    }

    // ---------------------------------------------------------------
    // Refill: parallel test for the analogous failure modes.
    // ---------------------------------------------------------------

    /// Build a fully-signed `KeyPackageRefillReq` carrying `records`.
    fn build_refill(
        owner: &SigningKey, records: Vec<KeyPackageRecord>, timestamp: u64,
    ) -> KeyPackageRefillReq {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, &records);
        let msg = kp_refill_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &digest,
            records.len() as u32,
            timestamp,
        );
        let sig = owner.sign(&msg);
        KeyPackageRefillReq {
            ipk: ipk.into(),
            records,
            timestamp,
            sig: sig.to_bytes().into(),
        }
    }

    #[test]
    fn refill_appends_to_existing_stash() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let rec_a = build_record(&owner, [0xA0; 32], b"a".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_a.clone()], now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        // Refill with a fresh record.
        let rec_b = build_record(&owner, [0xB0; 32], b"b".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_refill(&dht, build_refill(&owner, vec![rec_b.clone()], now), auth_peer, now),
            KeyPackageRefillOutcome::Appended
        );

        let stash = iterate_stash(&dht, &rec_a.ipk.0);
        assert_eq!(stash.len(), 2);
    }

    #[test]
    fn refill_with_static_fields_conflict_is_rejected() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let kp_ref = [0xCC; 32];
        let rec_a = build_record(&owner, kp_ref, b"original".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_publish(&dht, build_publish(&owner, vec![rec_a], now), auth_peer, now),
            KeyPackagePublishOutcome::Stored
        );

        // Refill with the same kp_ref but different bytes.
        let rec_b = build_record(&owner, kp_ref, b"FORGED".to_vec(), now + 60_000);
        assert_eq!(
            handle_keypackage_refill(&dht, build_refill(&owner, vec![rec_b], now), auth_peer, now),
            KeyPackageRefillOutcome::StaticFieldsConflict
        );
    }

    #[test]
    fn publish_with_too_many_records_is_rejected() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let recs: Vec<KeyPackageRecord> = (0..(KP_STASH_TARGET as u32 + 1))
            .map(|i| {
                let mut kp_ref = [0u8; 32];
                kp_ref[..4].copy_from_slice(&i.to_be_bytes());
                build_record(&owner, kp_ref, vec![1, 2, 3], now + 60_000)
            })
            .collect();
        let req = build_publish(&owner, recs, now);
        assert_eq!(
            handle_keypackage_publish(&dht, req, auth_peer, now),
            KeyPackagePublishOutcome::TooMany
        );
    }

    #[test]
    fn fetch_rejects_redirected_requester() {
        // A captured `KeyPackageFetch` signed for requester_a cannot
        // be replayed by requester_b. Mirrors the QueueFetch
        // cross-relay replay defence.
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();

        let req_a = NodeId::new([0xAA; 32]);
        let req_b = NodeId::new([0xBB; 32]);

        let target_ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let req = KeyPackageFetchReq {
            target_ipk: target_ipk.into(),
            requester_relay_id: req_a,
            timestamp: now,
        };
        // Authenticated peer is req_b — must reject (we use the
        // RateLimited soft response to avoid leaking presence).
        let outcome = handle_keypackage_fetch(&dht, req, req_b, now);
        assert!(matches!(outcome, KeyPackageFetchOutcome::RateLimited));
    }

    #[test]
    fn publish_outside_skew_window_is_rejected_as_bad_sig() {
        let owner = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let rec = build_record(&owner, [0xEE; 32], b"x".to_vec(), now + 120_000);
        // Sign at `now` but verify at `now + 2 minutes` — outside the
        // 60s skew window.
        let req = build_publish(&owner, vec![rec], now);
        let later = now + 120_000;
        assert_eq!(
            handle_keypackage_publish(&dht, req, auth_peer, later),
            KeyPackagePublishOutcome::BadSig
        );
    }
}
