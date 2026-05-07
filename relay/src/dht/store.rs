//! On-disk presence-record persistence, conflict resolution, and CF
//! lifecycle.
//!
//! This module owns the hot path through the `dht_presence` column
//! family: every inbound `Store` / `Tombstone` RPC funnels through
//! [`store_record`] / [`store_tombstone`], and every `FindValue` /
//! republish path looks up via [`lookup_record`].
//!
//! ## Wire vs storage type
//!
//! There is **no** separate storage type. The same
//! [`common::proto::dht_p2p::PresenceRecord`] / [`TombstoneRecord`] that
//! travels on the wire is postcard-encoded directly into the
//! `dht_presence` CF — keeping the formats merged means a future
//! protocol-version bump touches *one* place, not two.
//!
//! ## Conflict resolution
//!
//! Per §5.3, replicas keep the larger of `(self, incoming)` under the
//! ordering `generation` desc → `not_before` desc → `relay_id` lex desc.
//! That total order is implemented on
//! [`PresenceRecord::compare`](common::proto::dht_p2p::PresenceRecord::compare)
//! — we just call it.
//!
//! ## Tombstone keys
//!
//! Tombstones share the `dht_presence` CF but use a `tombstone_<ipk>`
//! prefix (per §1.2 paragraph "Tombstones") so a single point-get with
//! either prefix recovers the right record without a full scan. The
//! prefix is one byte (`TOMB_PREFIX`) followed by the 32-byte IPK.
//!
//! design-doc: §1.1 (PresenceRecord), §1.2 (RocksDB column families),
//! §1.1.2 (replay protection / clock skew window),
//! §1.1.3 (TTL and republish semantics),
//! §5.3 (multi-writer conflict resolution).

use common::proto::dht_p2p::PresenceRecord;
use common::proto::dht_p2p::PresenceVerifyError;
use common::proto::dht_p2p::StoreOutcome;
use common::proto::dht_p2p::TombstoneOutcome;
use common::proto::dht_p2p::TombstoneRecord;
use common::proto::dht_p2p::tombstone_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use ed25519_dalek::Signature;
use ed25519_dalek::Verifier;
use ed25519_dalek::VerifyingKey;
use rust_rocksdb::IteratorMode;
use rust_rocksdb::WriteOptions;

use super::Dht;
use super::config::K;
use super::config::PRESENCE_TTL_MS;

/// Column-family name for the `(user_ipk → PresenceRecord)` map this relay
/// holds as a replica. Also holds tombstones under a `TOMB_PREFIX`-prefixed
/// key.
///
/// design-doc: §1.2 — keyed by `[u8; 32]` user IPK; values are
/// postcard-encoded `PresenceRecord`. No prefix extractor (point lookups
/// only).
pub const CF_DHT_PRESENCE: &str = "dht_presence";

/// Column-family name for cached internal Merkle-tree node hashes.
///
/// design-doc: §1.2 — keys are `merkle_key = slice_id(1) || level(1) ||
/// index_within_level(1)`, values are 32-byte BLAKE3 hashes.
pub const CF_DHT_MERKLE: &str = "dht_merkle";

/// Column-family name for the per-recipient offline-message queue this
/// relay holds when it is in a user's k-closest set.
///
/// **Key shape** mirrors the existing `cf_messages` (default CF) layout:
/// `MessageKey { recipient: user_ipk, ts_ms, dispatch_id }` (56 bytes,
/// see [`crate::storage::MessageKey`]). Reusing the same type lets the
/// existing prefix-iterator / range-iterator helpers work unchanged for
/// per-recipient drains and per-recipient cap enforcement
/// ([`crate::storage::MAX_QUEUED_PER_RECIPIENT`]).
///
/// **Value**: postcard-encoded
/// [`common::proto::client_rel::DispatchP`]. The dispatch is stored
/// verbatim — its `sig` is the user's end-to-end signature and is
/// preserved unchanged so the recipient can verify the chain on drain.
///
/// **Prefix extractor**: 32-byte fixed prefix (matches the recipient
/// field at offset 0 in [`crate::storage::MessageKey`]). Same options
/// the default CF uses for its message queue — the two key spaces are
/// identical 56-byte tuples differing only in *which* relay holds them
/// (this CF: home-relay's k-closest queue; default CF: sender-relay's
/// fallback safety net).
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §6.1 (cf_dht_queue);
/// `misc/specs/DHT.md` §1.2 (CF taxonomy convention).
pub const CF_DHT_QUEUE: &str = "dht_queue";

/// Single-byte prefix that distinguishes tombstone entries from presence
/// records inside [`CF_DHT_PRESENCE`]. Records use a bare 32-byte IPK key;
/// tombstones use `TOMB_PREFIX || ipk` (33 bytes).
///
/// `0xFF` is chosen because no record IPK byte ever equals it as a *prefix*
/// in the bare-32-byte key form (the bare form is exactly 32 bytes long;
/// any 33-byte read with `0xFF` as byte 0 is unambiguously a tombstone).
const TOMB_PREFIX: u8 = 0xFF;

// ---------------------------------------------------------------------------
// Helpers — key construction
// ---------------------------------------------------------------------------

/// Tombstone key: `TOMB_PREFIX || user_ipk`.
fn tombstone_key(ipk: &[u8; 32]) -> [u8; 33] {
    let mut k = [0u8; 33];
    k[0] = TOMB_PREFIX;
    k[1..].copy_from_slice(ipk);
    k
}

/// Inspect a CF-key byte slice and decide whether it's a presence record
/// (32 bytes), a tombstone (33 bytes prefixed with [`TOMB_PREFIX`]), or
/// something we don't recognise (which we ignore — defensively).
enum KeyKind<'a> {
    Record(&'a [u8]),
    Tombstone(&'a [u8]),
    Unknown,
}

fn classify_key(k: &[u8]) -> KeyKind<'_> {
    match k.len() {
        32 => KeyKind::Record(k),
        33 if k[0] == TOMB_PREFIX => KeyKind::Tombstone(&k[1..]),
        _ => KeyKind::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Helpers — verification & ownership
// ---------------------------------------------------------------------------

/// Verify a tombstone end-to-end:
///
/// 1. `relay_id == BLAKE3(relay_pubkey)` (binds id to pubkey).
/// 2. `relay_pubkey` parses as a verifying Ed25519 key.
/// 3. The Ed25519 signature verifies over the canonical
///    [`tombstone_signing_input`] transcript.
///
/// Returns `Ok(())` on success; matched onto `TombstoneOutcome::BadSig`
/// at the caller site for any failure (we do not differentiate
/// id-mismatch from a forged signature on the wire — both are a
/// "rejected" outcome from the requester's perspective).
fn verify_tombstone(tomb: &TombstoneRecord) -> Result<(), TombstoneVerifyError> {
    // 1. id-to-pubkey binding.
    let derived = NodeId::new(tomb.relay_pubkey.as_ref());
    if derived != tomb.relay_id {
        return Err(TombstoneVerifyError::RelayIdMismatch);
    }

    // 2. Pubkey parse.
    let vk = VerifyingKey::from_bytes(&tomb.relay_pubkey.0)
        .map_err(|_| TombstoneVerifyError::MalformedRelayPubkey)?;

    // 3. Signature verification.
    let sig = Signature::from_bytes(&tomb.relay_sig.0);
    let msg = tombstone_signing_input(
        &tomb.user_ipk.0,
        &tomb.relay_id,
        &tomb.relay_pubkey.0,
        tomb.generation,
        tomb.deleted_at,
    );
    vk.verify(&msg, &sig).map_err(|_| TombstoneVerifyError::BadRelaySig)?;

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum TombstoneVerifyError {
    BadRelaySig,
    MalformedRelayPubkey,
    RelayIdMismatch,
}

/// Are we — `dht.self_id` — among the k closest known nodes to `target`?
///
/// Implemented as: ask the routing table for its top-(k+1) closest peers,
/// then compute the same XOR distance for self and check that self's
/// distance is `<=` the kth peer's distance. We use `k+1` rather than
/// `k` so that even if the routing table is fully populated and the kth
/// position is *exactly* equal to self in distance, we don't get pushed
/// out by a sort-tiebreak.
///
/// **Caveat (lock):** holds the routing-table read lock briefly to clone
/// out the candidate descriptors; never across an `await` (this function
/// is sync).
///
/// design-doc: §5.1 (key→value, single relay per user) + §5.4 (ownership
/// shifts handled lazily).
fn self_is_owner(dht: &Dht, target: &[u8; 32]) -> bool {
    let target_id = NodeId::from_bytes(*target);
    // Compare distances on raw 32-byte XOR; a Vec is fine because the
    // routing table at most has K+1 entries here.
    let candidates = dht.routing.read().find_closest(&target_id, K + 1);

    let self_dist = xor32(dht.node_id.as_bytes(), target);

    if candidates.len() < K {
        // Not enough peers known yet — be permissive. This matches the
        // §3.5 bootstrap "Ready is non-strict" stance: a relay that just
        // came online is allowed to accept stores even before its
        // routing table is dense, otherwise we couldn't seed a fresh
        // network.
        return true;
    }

    // Find the k-th closest peer's distance (zero-indexed: index K-1).
    let kth_dist = xor32(candidates[K - 1].id.as_bytes(), target);
    self_dist <= kth_dist
}

fn xor32(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

// ---------------------------------------------------------------------------
// Public API — store / lookup / evict
// ---------------------------------------------------------------------------

/// Persist an inbound `Store`'s `PresenceRecord` against any record this
/// replica already holds, applying §5.3 conflict resolution.
///
/// Returns the `StoreOutcome` to ship back over the wire:
///
/// - [`StoreOutcome::Stored`] — accepted (either fresh insert or strictly
///   newer than what we had).
/// - [`StoreOutcome::Stale`] — we already hold a record that wins under
///   §5.3; the new one is dropped, our local state is unchanged.
/// - [`StoreOutcome::NotOwner`] — `dht.self_id` is not in the k closest
///   to `record.user_ipk` per the current routing table.
/// - [`StoreOutcome::BadSig`] — `record.verify(now)` failed on either the
///   user_sig or relay_sig.
/// - [`StoreOutcome::TtlExpired`] — `record.verify(now)` failed because
///   the record is past `not_after` or the future-skew window.
///
/// Durability: the put uses `WriteOptions::set_sync(true)` so the WAL
/// fsyncs before we return — same pattern as the message queue's
/// `store_in_rocks` (`relay/src/quic/handler/client/events/forward.rs`).
///
/// design-doc: §1.1.2 (replay protection), §5.3 (conflict resolution),
/// §8.4 (`NotOwner` is the storage-flooding defence).
pub(crate) fn store_record(dht: &Dht, record: PresenceRecord, now_ms: u64) -> StoreOutcome {
    dht.metrics.inc_stores_received();

    // 1. End-to-end verify (sigs + clock window + structural).
    if let Err(e) = record.verify(now_ms) {
        let outcome = match e {
            PresenceVerifyError::Expired | PresenceVerifyError::NotYetValid => {
                StoreOutcome::TtlExpired
            }
            // Every other variant is "the record is structurally bad" —
            // collapse onto BadSig per §2.4.4 wire semantics. The
            // `NotYetValid`/`Expired` carve-out exists because §2.5
            // names a separate close code (`DhtClockSkew`) for it.
            _ => StoreOutcome::BadSig,
        };
        dht.metrics.inc_stores_rejected();
        return outcome;
    }

    // 2. Ownership check.
    if !self_is_owner(dht, &record.user_ipk.0) {
        dht.metrics.inc_stores_rejected();
        return StoreOutcome::NotOwner;
    }

    // 3. Conflict resolution.
    let key = record.user_ipk.0;
    let cf = match dht.rocks.cf_handle(CF_DHT_PRESENCE) {
        Some(cf) => cf,
        None => {
            // Should be impossible — Dht::new verifies the CF exists at
            // construction. Guard anyway so a partial-init bug surfaces
            // as a soft error rather than a process panic.
            dht.metrics.inc_stores_rejected();
            return StoreOutcome::BadSig;
        }
    };

    if let Ok(Some(existing_bytes)) = dht.rocks.get_cf(&cf, key) {
        if let Ok(existing) = PresenceRecord::deser(&existing_bytes) {
            match record.compare(&existing) {
                std::cmp::Ordering::Greater => {
                    // New record wins — fall through to write.
                }
                std::cmp::Ordering::Equal => {
                    // Byte-identical → idempotent re-store. Treat as
                    // "Stored" so the caller doesn't waste a retry,
                    // but no write is needed. (We still rewrite below
                    // for simplicity / fsync-driven freshness.)
                }
                std::cmp::Ordering::Less => {
                    // Existing wins — caller's record is stale.
                    dht.metrics.inc_stores_rejected();
                    return StoreOutcome::Stale;
                }
            }
        }
        // If we couldn't deserialize the existing entry, treat the slot
        // as empty: better to overwrite a corrupted record than to wedge
        // forever.
    }

    // 4. Persist with fsync.
    let bytes = match record.ser() {
        Ok(b) => b,
        Err(_) => {
            dht.metrics.inc_stores_rejected();
            return StoreOutcome::BadSig;
        }
    };

    let mut wopts = WriteOptions::default();
    wopts.set_sync(true);
    if dht.rocks.put_cf_opt(&cf, key, &bytes, &wopts).is_err() {
        dht.metrics.inc_stores_rejected();
        return StoreOutcome::BadSig;
    }

    // 5. Update the Merkle anti-entropy state. This is in-process only
    //    (the §6.1 design accepts that the Merkle CF is a cache of the
    //    record CF — we rebuild on restart from `cf_dht_presence`).
    //    Hold the merkle write lock briefly; never across an await
    //    (this whole function is sync).
    //
    //    Reuse the bytes we just serialised for the put — saves a
    //    second postcard pass.
    //
    //    design-doc: §6.1 (per-slice Merkle tree update on every accepted
    //    record).
    let vh = super::sync::record_value_hash(&bytes);
    {
        let mut merkle = dht.merkle.write();
        merkle.insert(&key, vh);
    }

    dht.metrics.inc_stores_accepted();
    StoreOutcome::Stored
}

/// Persist a tombstone, deleting any record it supersedes.
///
/// Conflict rule (mirrors §5.3 in reverse): a tombstone with `generation
/// >= existing.generation` supersedes the record. We delete the record
/// from `cf_presence` and write the tombstone under
/// [`tombstone_key`]. A tombstone with `generation < existing.generation`
/// is rejected as `Stale`.
///
/// design-doc: §1.2 (Tombstones — honoured for `2 × PRESENCE_TTL_MS`).
pub(crate) fn store_tombstone(
    dht: &Dht, tomb: TombstoneRecord, _now_ms: u64,
) -> TombstoneOutcome {
    // 1. Verify the tombstone's relay signature.
    if verify_tombstone(&tomb).is_err() {
        return TombstoneOutcome::BadSig;
    }

    // 2. Ownership.
    if !self_is_owner(dht, &tomb.user_ipk.0) {
        return TombstoneOutcome::NotOwner;
    }

    let cf = match dht.rocks.cf_handle(CF_DHT_PRESENCE) {
        Some(cf) => cf,
        None => return TombstoneOutcome::BadSig,
    };

    // 3. Compare against any existing record — only delete if the
    //    tombstone's generation is `>=`.
    let record_key = tomb.user_ipk.0;
    if let Ok(Some(existing_bytes)) = dht.rocks.get_cf(&cf, record_key) {
        if let Ok(existing) = PresenceRecord::deser(&existing_bytes) {
            if tomb.generation < existing.generation {
                return TombstoneOutcome::Stale;
            }
        }
    }

    // 4. Compare against any existing tombstone — keep higher generation.
    let tk = tombstone_key(&tomb.user_ipk.0);
    if let Ok(Some(existing_tomb_bytes)) = dht.rocks.get_cf(&cf, tk) {
        if let Ok(existing_tomb) = TombstoneRecord::deser(&existing_tomb_bytes) {
            if tomb.generation < existing_tomb.generation {
                return TombstoneOutcome::Stale;
            }
        }
    }

    let bytes = match tomb.ser() {
        Ok(b) => b,
        Err(_) => return TombstoneOutcome::BadSig,
    };

    let mut wopts = WriteOptions::default();
    wopts.set_sync(true);

    // 5. Atomic-ish: delete the record then write the tombstone. RocksDB
    //    doesn't expose a transaction handle on the bare `DB`, but in
    //    the non-transactional case we accept that a crash between the
    //    two operations leaves us with the (resurrected) record. The
    //    next anti-entropy round (§6.3) re-converges by replaying the
    //    same tombstone from a peer.
    let _ = dht.rocks.delete_cf(&cf, record_key);
    if dht.rocks.put_cf_opt(&cf, tk, &bytes, &wopts).is_err() {
        return TombstoneOutcome::BadSig;
    }

    // 6. Advertise the tombstone via Merkle (phase 1h, item 6 of the
    //    DoS-hardening dispatch). Insert the tombstone's value-hash
    //    *under the same IPK key* as the live record would have been —
    //    the leaf hash is order-sensitive on its `(ipk, value_hash)`
    //    entries, and `tombstone_value_hash` carries a distinct
    //    domain tag (`MERKLE_TOMBSTONE_DOMAIN`) so the tombstone-leaf
    //    hash for `(ipk, gen)` cannot collide with the record-leaf
    //    hash for the same `(ipk, gen)`. A peer still holding the live
    //    record sees a root divergence on this slice → bisect →
    //    FetchRecord → we return the tombstone in the new
    //    `FetchRecordResp::tombstones` field (also phase 1h) → peer
    //    applies it via `store_tombstone` and converges.
    //
    //    We `insert` rather than `remove` so anti-entropy converges on
    //    deletions. The eventual GC of tombstones at `2 ×
    //    PRESENCE_TTL_MS` (`evict_expired`) calls `merkle.remove`
    //    explicitly so the leaf disappears from the bitset only after
    //    the honour window has expired and no peer can resurrect.
    //
    //    design-doc: §1.2 (Tombstones honoured for `2 × PRESENCE_TTL_MS`),
    //    §6.3 ("Tombstones converge the same way").
    let vh = super::sync::tombstone_value_hash(&bytes);
    {
        let mut merkle = dht.merkle.write();
        merkle.insert(&tomb.user_ipk.0, vh);
    }

    TombstoneOutcome::Stored
}

/// Look up the local replica's `PresenceRecord` for `user_ipk`. Returns
/// `None` if no record is stored, or if the record exists but has
/// expired (in which case we delete it opportunistically).
///
/// Used by:
/// - `FindValue` inbound RPC (handler.rs) — when the responder *is* in
///   the k closest, this is the primary lookup.
/// - The publish path (publish.rs) — when self is in the k closest,
///   we self-store via `store_record` and re-read here.
///
/// design-doc: §4.2 (Found / NotPresent), §1.1.3 (TTL).
pub(crate) fn lookup_record(
    dht: &Dht, user_ipk: &[u8; 32], now_ms: u64,
) -> Option<PresenceRecord> {
    let cf = dht.rocks.cf_handle(CF_DHT_PRESENCE)?;
    let bytes = dht.rocks.get_cf(&cf, user_ipk).ok().flatten()?;
    let record = PresenceRecord::deser(&bytes).ok()?;

    // Verify TTL (don't bother re-running signature checks — those were
    // done at store time; if we were tricked then, re-verifying here
    // doesn't help). Expired records are deleted opportunistically so
    // a busy `FindValue` path doesn't keep returning them.
    match record.verify(now_ms) {
        Ok(()) => Some(record),
        Err(_) => {
            // Best-effort cleanup; ignore any error.
            let _ = dht.rocks.delete_cf(&cf, user_ipk);
            None
        }
    }
}

/// Periodic cleanup pass: scan `cf_presence` and delete:
/// 1. Expired *records* (whose `not_after <= now_ms`), and
/// 2. Expired *tombstones* (whose `deleted_at + 2 × PRESENCE_TTL_MS <
///    now_ms` — the §1.1.3 / §1.2 honour window has fully elapsed).
///
/// The 2× window for tombstones is deliberate: §1.1.3 says replicas
/// honour a tombstone for that long *after* `deleted_at`. By the time
/// 2× TTL has passed, no peer in the network can still hold a stale
/// live record they could resurrect — they would have hit `not_after`
/// long before. Dropping the tombstone after that is safe.
///
/// When a tombstone is deleted, its leaf is also removed from the
/// in-memory Merkle tree (phase 1h, item 6). The leaf was advertised
/// during the honour window so anti-entropy could carry the deletion;
/// once the window expires, the leaf disappears from the slice's bitset
/// and a new live record for the same IPK can re-occupy the leaf
/// without diverging from a peer that GC'd theirs first.
///
/// Returns the number of entries evicted (records + tombstones).
///
/// **Caller responsibility:** this is meant to be called from a periodic
/// scheduler (phase 1g's anti-entropy task), not on a hot RPC path. A
/// full CF scan is `O(records held)` which is small per relay (~300
/// records at design-doc §6.4 scale) but still costs an iterator open.
///
/// design-doc: §1.1.2 (`now > not_after` rejection), §1.1.3 / §1.2
/// (tombstone honour window = `2 × PRESENCE_TTL_MS`), §6.3 (anti-entropy
/// of tombstones).
pub fn evict_expired(dht: &Dht, now_ms: u64) -> usize {
    let Some(cf) = dht.rocks.cf_handle(CF_DHT_PRESENCE) else {
        return 0;
    };

    let tomb_horizon = 2 * PRESENCE_TTL_MS;

    let mut victims: Vec<Vec<u8>> = Vec::new();
    let mut tomb_victim_ipks: Vec<[u8; 32]> = Vec::new();
    for entry in dht.rocks.iterator_cf(&cf, IteratorMode::Start) {
        let (key, value) = match entry {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        match classify_key(&key) {
            KeyKind::Record(_) => {
                if let Ok(rec) = PresenceRecord::deser(&value) {
                    if now_ms >= rec.not_after {
                        victims.push(key.to_vec());
                    }
                }
            }
            KeyKind::Tombstone(ipk_slice) => {
                // Honour-window check: drop only if the wall clock has
                // moved past `deleted_at + 2 × PRESENCE_TTL_MS`.
                if let Ok(t) = TombstoneRecord::deser(&value) {
                    let cutoff = t.deleted_at.saturating_add(tomb_horizon);
                    if now_ms >= cutoff {
                        victims.push(key.to_vec());
                        // Snapshot the IPK so we can remove the
                        // Merkle leaf below — `t.user_ipk.0` is also
                        // the same value as `ipk_slice`, but going via
                        // the parsed record avoids re-validating the
                        // 33-byte key shape.
                        let mut ipk = [0u8; 32];
                        ipk.copy_from_slice(&t.user_ipk.0);
                        // Sanity: classified-slice and parsed IPK
                        // should agree; if not, prefer the parsed
                        // one (the value is what we hashed).
                        debug_assert_eq!(ipk_slice, &ipk);
                        tomb_victim_ipks.push(ipk);
                    }
                }
            }
            KeyKind::Unknown => {}
        }
    }

    let mut evicted = 0;
    for k in victims {
        if dht.rocks.delete_cf(&cf, k).is_ok() {
            evicted += 1;
        }
    }
    if !tomb_victim_ipks.is_empty() {
        // Drop the Merkle leaves for all GC'd tombstones in a single
        // write-guard scope; never held across an `await` (this whole
        // function is sync).
        let mut merkle = dht.merkle.write();
        for ipk in &tomb_victim_ipks {
            merkle.remove(ipk);
        }
    }
    evicted
}

/// Look up a tombstone by IPK. Returns `None` if no tombstone is stored.
/// Used by anti-entropy (phase 1g) to decide whether a peer's apparent
/// "missing record" is genuinely gone or just stale.
#[allow(dead_code)] // Consumed by phase 1g's sync RPC handlers.
pub(crate) fn lookup_tombstone(dht: &Dht, user_ipk: &[u8; 32]) -> Option<TombstoneRecord> {
    let cf = dht.rocks.cf_handle(CF_DHT_PRESENCE)?;
    let key = tombstone_key(user_ipk);
    let bytes = dht.rocks.get_cf(&cf, key).ok().flatten()?;
    TombstoneRecord::deser(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::dht_p2p::PresenceRecord;
    use common::proto::dht_p2p::StoreOutcome;
    use common::proto::dht_p2p::TombstoneOutcome;
    use common::proto::dht_p2p::TombstoneRecord;
    use common::proto::dht_p2p::presence_record_relay_signing_input;
    use common::proto::dht_p2p::presence_record_user_signing_input;
    use common::proto::dht_p2p::tombstone_signing_input;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;
    use crate::dht::dht_cf_descriptors;

    /// Deterministic-distinct seed counter so `fresh_signing_key()` calls
    /// return distinct ids without an RNG dep.
    ///
    /// Tests don't need cryptographic randomness — they need *distinct*
    /// keypairs. `from_bytes` lets us derive a key from a counter-bumped
    /// seed cheaply.
    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        // Spread non-zero bytes throughout the seed so two consecutive
        // counter values yield very different Ed25519 secret scalars.
        seed[31] = (n & 0xff) as u8;
        seed[16] = ((n >> 8) & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    /// Build a `Dht` instance backed by a fresh tempdir RocksDB. The
    /// DB lives in `/tmp` so the test doesn't pollute the workspace
    /// (each test gets its own subdir keyed off a counter).
    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-dht-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let mut opts = rust_rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let mut cfs = vec![rust_rocksdb::ColumnFamilyDescriptor::new(
            "default",
            rust_rocksdb::Options::default(),
        )];
        cfs.extend(dht_cf_descriptors());

        let db = rust_rocksdb::DB::open_cf_descriptors(&opts, &path, cfs).expect("open db");
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, Arc::new(db)).expect("dht"))
    }

    fn build_record(
        user: &SigningKey, relay: &SigningKey, generation: u64, not_before: u64, ttl_ms: u64,
    ) -> PresenceRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let not_after = not_before + ttl_ms;
        let capabilities: u16 = 0;

        let user_msg = presence_record_user_signing_input(&user_ipk, &relay_id, generation);
        let user_sig = user.sign(&user_msg);

        let relay_msg = presence_record_relay_signing_input(
            &user_ipk,
            &relay_id,
            &relay_pubkey,
            not_before,
            not_after,
            generation,
            capabilities,
            &user_sig.to_bytes(),
        );
        let relay_sig = relay.sign(&relay_msg);

        PresenceRecord {
            user_ipk: user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            not_before,
            not_after,
            generation,
            capabilities,
            user_sig: user_sig.to_bytes().into(),
            relay_sig: relay_sig.to_bytes().into(),
        }
    }

    fn build_tombstone(
        user: &SigningKey, relay: &SigningKey, generation: u64, deleted_at: u64,
    ) -> TombstoneRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);

        let msg = tombstone_signing_input(
            &user_ipk,
            &relay_id,
            &relay_pubkey,
            generation,
            deleted_at,
        );
        let sig = relay.sign(&msg);

        TombstoneRecord {
            user_ipk: user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            generation,
            deleted_at,
            relay_sig: sig.to_bytes().into(),
        }
    }

    #[test]
    fn store_record_round_trip() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 600_000);

        let outcome = store_record(&dht, rec.clone(), now + 1);
        assert_eq!(outcome, StoreOutcome::Stored);

        let got = lookup_record(&dht, &rec.user_ipk.0, now + 1).expect("present");
        assert_eq!(got, rec);
    }

    #[test]
    fn store_record_higher_gen_replaces_lower() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let r1 = build_record(&user, &relay, 1, now, 600_000);
        let r2 = build_record(&user, &relay, 2, now, 600_000);

        assert_eq!(store_record(&dht, r1.clone(), now + 1), StoreOutcome::Stored);
        assert_eq!(store_record(&dht, r2.clone(), now + 1), StoreOutcome::Stored);

        let got = lookup_record(&dht, &r2.user_ipk.0, now + 1).expect("present");
        assert_eq!(got.generation, 2);
    }

    #[test]
    fn store_record_lower_gen_rejected_as_stale() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let r1 = build_record(&user, &relay, 1, now, 600_000);
        let r2 = build_record(&user, &relay, 2, now, 600_000);

        assert_eq!(store_record(&dht, r2.clone(), now + 1), StoreOutcome::Stored);
        assert_eq!(store_record(&dht, r1.clone(), now + 1), StoreOutcome::Stale);

        // Verify gen=2 is still the persisted one.
        let got = lookup_record(&dht, &r1.user_ipk.0, now + 1).expect("present");
        assert_eq!(got.generation, 2);
    }

    #[test]
    fn store_record_tampered_fails_bad_sig() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let mut rec = build_record(&user, &relay, 1, now, 600_000);
        // Tamper with not_after — that field is covered by relay_sig.
        rec.not_after += 1;

        assert_eq!(store_record(&dht, rec, now + 1), StoreOutcome::BadSig);
    }

    #[test]
    fn store_record_expired_fails_ttl() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        // ttl=1 ms; we evaluate well past not_after.
        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 1);
        assert_eq!(store_record(&dht, rec, now + 1_000), StoreOutcome::TtlExpired);
    }

    #[test]
    fn evict_expired_removes_only_expired_records() {
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        // One record with a 1-second TTL, one with a 10-minute TTL.
        let user_a = fresh_signing_key();
        let user_b = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let short = build_record(&user_a, &relay, 1, now, 1_000);
        let long = build_record(&user_b, &relay, 1, now, 600_000);

        assert_eq!(store_record(&dht, short.clone(), now + 1), StoreOutcome::Stored);
        assert_eq!(store_record(&dht, long.clone(), now + 1), StoreOutcome::Stored);

        // Skip past `short.not_after` but well before `long.not_after`.
        let evicted = evict_expired(&dht, now + 5_000);
        assert_eq!(evicted, 1);

        // Verify the long record survived.
        assert!(lookup_record(&dht, &long.user_ipk.0, now + 5_000).is_some());
        assert!(lookup_record(&dht, &short.user_ipk.0, now + 5_000).is_none());
    }

    #[test]
    fn lookup_record_returns_none_for_expired() {
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 1_000);
        assert_eq!(store_record(&dht, rec.clone(), now + 1), StoreOutcome::Stored);

        // After expiry, lookup_record returns None *and* deletes.
        assert!(lookup_record(&dht, &rec.user_ipk.0, now + 5_000).is_none());
        let cf = dht.rocks.cf_handle(CF_DHT_PRESENCE).unwrap();
        let bytes = dht.rocks.get_cf(&cf, rec.user_ipk.0).unwrap();
        assert!(bytes.is_none(), "expired record should have been deleted");
    }

    #[test]
    fn store_tombstone_supersedes_record_at_same_or_higher_gen() {
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 5, now, 600_000);
        assert_eq!(store_record(&dht, rec.clone(), now + 1), StoreOutcome::Stored);

        let tomb = build_tombstone(&user, &relay, 5, now + 100);
        assert_eq!(store_tombstone(&dht, tomb.clone(), now + 100), TombstoneOutcome::Stored);

        // Record gone.
        assert!(lookup_record(&dht, &rec.user_ipk.0, now + 100).is_none());
        // Tombstone present.
        let got = lookup_tombstone(&dht, &rec.user_ipk.0).expect("tombstone present");
        assert_eq!(got.generation, 5);
    }

    #[test]
    fn store_tombstone_with_lower_gen_rejected_as_stale() {
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 5, now, 600_000);
        assert_eq!(store_record(&dht, rec.clone(), now + 1), StoreOutcome::Stored);

        let tomb_old = build_tombstone(&user, &relay, 4, now + 100);
        assert_eq!(
            store_tombstone(&dht, tomb_old, now + 100),
            TombstoneOutcome::Stale
        );

        // Record survived.
        assert!(lookup_record(&dht, &rec.user_ipk.0, now + 100).is_some());
    }

    #[test]
    fn evict_expired_keeps_tombstones_inside_honour_window() {
        // Phase 1h, item 4: tombstones are honoured for 2 × TTL after
        // their `deleted_at`. Inside that window `evict_expired` must
        // leave them alone.
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let tomb = build_tombstone(&user, &relay, 1, now);
        assert_eq!(store_tombstone(&dht, tomb.clone(), now), TombstoneOutcome::Stored);

        // 2 × PRESENCE_TTL_MS minus a millisecond — still inside the
        // honour window.
        let evict_at = now + 2 * super::super::config::PRESENCE_TTL_MS - 1;
        let evicted = evict_expired(&dht, evict_at);
        assert_eq!(evicted, 0);

        let still_there =
            lookup_tombstone(&dht, &tomb.user_ipk.0).expect("tombstone still present");
        assert_eq!(still_there.generation, 1);
    }

    #[test]
    fn evict_expired_drops_tombstones_past_honour_window() {
        // Phase 1h, item 4: once `deleted_at + 2 × PRESENCE_TTL_MS` has
        // elapsed, the tombstone should be GC'd — both the on-disk
        // entry and its Merkle leaf (so the slice bitset stops
        // advertising it).
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let tomb = build_tombstone(&user, &relay, 1, now);
        assert_eq!(store_tombstone(&dht, tomb.clone(), now), TombstoneOutcome::Stored);
        // Confirm the Merkle tree now advertises this tombstone (item
        // 6 — store_tombstone inserts the value-hash into the slice).
        let slice_id = tomb.user_ipk.0[0];
        assert_ne!(dht.merkle.read().root(slice_id), [0u8; 32]);

        // Past the honour window by one ms.
        let evict_at = now + 2 * super::super::config::PRESENCE_TTL_MS + 1;
        let evicted = evict_expired(&dht, evict_at);
        assert_eq!(evicted, 1);

        // Tombstone gone from disk and Merkle tree.
        assert!(lookup_tombstone(&dht, &tomb.user_ipk.0).is_none());
        assert_eq!(dht.merkle.read().root(slice_id), [0u8; 32]);
    }

    #[test]
    fn store_tombstone_advertises_via_merkle() {
        // Phase 1h, item 6: storing a tombstone must update the Merkle
        // tree (insert with tombstone domain), not remove the leaf
        // entry — so anti-entropy carries deletions.
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        // Empty start state.
        let slice_id = NodeId::new(user.verifying_key().to_bytes()).as_bytes()[0];
        let _ = slice_id; // sanity: the slice is whatever the IPK's first byte is

        let tomb = build_tombstone(&user, &relay, 1, now);
        let user_slice = tomb.user_ipk.0[0];
        assert_eq!(dht.merkle.read().root(user_slice), [0u8; 32]);

        assert_eq!(store_tombstone(&dht, tomb.clone(), now), TombstoneOutcome::Stored);
        // Now non-zero — the tombstone leaf populated the slice.
        assert_ne!(dht.merkle.read().root(user_slice), [0u8; 32]);
    }

    #[test]
    fn record_then_tombstone_changes_merkle_root() {
        // The leaf hash for a record vs the leaf hash for a tombstone
        // differ by domain tag (`MERKLE_RECORD_DOMAIN` vs
        // `MERKLE_TOMBSTONE_DOMAIN`). Storing one then the other for
        // the same IPK must produce two different roots in sequence.
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(store_record(&dht, rec.clone(), now + 1), StoreOutcome::Stored);
        let user_slice = rec.user_ipk.0[0];
        let root_after_record = dht.merkle.read().root(user_slice);
        assert_ne!(root_after_record, [0u8; 32]);

        let tomb = build_tombstone(&user, &relay, 1, now + 100);
        assert_eq!(store_tombstone(&dht, tomb, now + 100), TombstoneOutcome::Stored);
        let root_after_tomb = dht.merkle.read().root(user_slice);
        assert_ne!(root_after_tomb, [0u8; 32]);
        assert_ne!(root_after_record, root_after_tomb);
    }
}
