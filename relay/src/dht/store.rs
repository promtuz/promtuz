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
//! Replicas keep the larger of `(self, incoming)` under the ordering
//! `generation` desc → `not_before` desc → `relay_id` lex desc. That
//! total order is implemented on
//! [`PresenceRecord::compare`](common::proto::dht_p2p::PresenceRecord::compare)
//! — we just call it.
//!
//! ## Tombstone keys
//!
//! Tombstones share the `dht_presence` CF but use a `tombstone_<ipk>`
//! prefix so a single point-get with either prefix recovers the right
//! record without a full scan. The prefix is one byte (`TOMB_PREFIX`)
//! followed by the 32-byte IPK.

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::dht_p2p::PresenceRecord;
use common::proto::dht_p2p::PresenceVerifyError;
use common::proto::dht_p2p::StoreOutcome;
use common::proto::dht_p2p::TombstoneOutcome;
use common::proto::dht_p2p::TombstoneRecord;
use common::proto::dht_p2p::tombstone_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;
use ed25519_dalek::Signature;
use ed25519_dalek::Verifier;
use ed25519_dalek::VerifyingKey;

use super::Dht;
use super::config::K;
use super::config::PRESENCE_TTL_MS;
use crate::storage::MAX_QUEUED_PER_RECIPIENT;
use crate::storage::MessageKey;

/// Single-byte prefix that distinguishes tombstone entries from presence
/// records inside the `dht_presence` keyspace. Records use a bare 32-byte IPK key;
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
/// One-line wrapper that defers to the canonical
/// [`super::routing::self_in_top_k`] helper — see that fn for the
/// permissive-sparse-table policy and the `K + 1` query rationale.
fn self_is_owner(dht: &Dht, target: &[u8; 32]) -> bool {
    super::routing::self_in_top_k(dht, &NodeId::from_bytes(*target))
}

// ---------------------------------------------------------------------------
// Public API — store / lookup / evict
// ---------------------------------------------------------------------------

/// Persist an inbound `Store`'s `PresenceRecord` against any record this
/// replica already holds, applying conflict resolution.
///
/// Returns the `StoreOutcome` to ship back over the wire:
///
/// - [`StoreOutcome::Stored`] — accepted (either fresh insert or strictly
///   newer than what we had).
/// - [`StoreOutcome::Stale`] — we already hold a record that wins under
///   conflict resolution; the new one is dropped.
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
pub(crate) fn store_record(dht: &Dht, record: PresenceRecord, now_ms: u64) -> StoreOutcome {
    dht.metrics.inc_stores_received();

    // 1. End-to-end verify (sigs + clock window + structural).
    if let Err(e) = record.verify(now_ms) {
        let outcome = match e {
            PresenceVerifyError::Expired | PresenceVerifyError::NotYetValid => {
                StoreOutcome::TtlExpired
            }
            // Every other variant is "the record is structurally bad" —
            // collapse onto BadSig. The `NotYetValid`/`Expired` carve-out
            // exists because a separate close code (`DhtClockSkew`)
            // applies to those.
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

    if let Ok(Some(existing_bytes)) = dht.store.presence.get(key)
        && let Ok(existing) = PresenceRecord::deser(&existing_bytes) {
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

    // 4. Persist with fsync.
    let bytes = match record.ser() {
        Ok(b) => b,
        Err(_) => {
            dht.metrics.inc_stores_rejected();
            return StoreOutcome::BadSig;
        }
    };

    if dht.store.put_sync(&dht.store.presence, key, &bytes).is_err() {
        dht.metrics.inc_stores_rejected();
        return StoreOutcome::BadSig;
    }

    // 5. Update the Merkle anti-entropy state. In-process only — the
    //    Merkle tree is a cache of the presence keyspace, rebuilt on restart
    //    from `dht_presence`. Hold the write lock briefly; never across
    //    an await (this whole function is sync).
    //    Reuse the bytes we just serialised — saves a second postcard pass.
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
/// A tombstone with `generation >= existing.generation` supersedes the
/// record. We delete the record from `cf_presence` and write the
/// tombstone under [`tombstone_key`]. A tombstone with
/// `generation < existing.generation` is rejected as `Stale`. Tombstones
/// are honoured for `2 × PRESENCE_TTL_MS`.
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

    // 3. Compare against any existing record — only delete if the
    //    tombstone's generation is `>=`.
    let record_key = tomb.user_ipk.0;
    if let Ok(Some(existing_bytes)) = dht.store.presence.get(record_key)
        && let Ok(existing) = PresenceRecord::deser(&existing_bytes)
            && tomb.generation < existing.generation {
                return TombstoneOutcome::Stale;
            }

    // 4. Compare against any existing tombstone — keep higher generation.
    let tk = tombstone_key(&tomb.user_ipk.0);
    if let Ok(Some(existing_tomb_bytes)) = dht.store.presence.get(tk)
        && let Ok(existing_tomb) = TombstoneRecord::deser(&existing_tomb_bytes)
            && tomb.generation < existing_tomb.generation {
                return TombstoneOutcome::Stale;
            }

    let bytes = match tomb.ser() {
        Ok(b) => b,
        Err(_) => return TombstoneOutcome::BadSig,
    };

    // 5. Atomic-ish: delete the record then write the tombstone. We don't
    //    wrap the pair in a fjall batch — a crash between the two ops leaves
    //    the (resurrected) record, and the next anti-entropy round
    //    re-converges by replaying the same tombstone from a peer.
    let _ = dht.store.presence.remove(record_key);
    if dht.store.put_sync(&dht.store.presence, tk, &bytes).is_err() {
        return TombstoneOutcome::BadSig;
    }

    // 6. Advertise the tombstone via Merkle. Insert the tombstone's
    //    value-hash *under the same IPK key* as the live record would
    //    have been —
    //    the leaf hash is order-sensitive on its `(ipk, value_hash)`
    //    entries, and `tombstone_value_hash` carries a distinct
    //    domain tag (`MERKLE_TOMBSTONE_DOMAIN`) so the tombstone-leaf
    //    hash for `(ipk, gen)` cannot collide with the record-leaf
    //    hash for the same `(ipk, gen)`. A peer still holding the live
    //    record sees a root divergence on this slice → bisect →
    //    FetchRecord → we return the tombstone in the
    //    `FetchRecordResp::tombstones` field → peer applies it via
    //    `store_tombstone` and converges.
    //
    //    We `insert` rather than `remove` so anti-entropy converges on
    //    deletions. The eventual GC of tombstones at `2 ×
    //    PRESENCE_TTL_MS` (`evict_expired`) calls `merkle.remove`
    //    explicitly so the leaf disappears from the bitset only after
    //    the honour window has expired and no peer can resurrect.
    //
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
pub(crate) fn lookup_record(
    dht: &Dht, user_ipk: &[u8; 32], now_ms: u64,
) -> Option<PresenceRecord> {
    let bytes = dht.store.presence.get(user_ipk).ok().flatten()?;
    let record = PresenceRecord::deser(&bytes).ok()?;

    // Verify TTL (don't bother re-running signature checks — those were
    // done at store time; if we were tricked then, re-verifying here
    // doesn't help). Expired records are deleted opportunistically so
    // a busy `FindValue` path doesn't keep returning them.
    match record.verify(now_ms) {
        Ok(()) => Some(record),
        Err(_) => {
            // Best-effort cleanup; ignore any error.
            let _ = dht.store.presence.remove(user_ipk);
            None
        }
    }
}

/// Periodic cleanup pass: scan `cf_presence` and delete:
/// 1. Expired *records* (whose `not_after <= now_ms`), and
/// 2. Expired *tombstones* (whose `deleted_at + 2 × PRESENCE_TTL_MS <
///    now_ms` — the honour window has fully elapsed).
///
/// The 2× window for tombstones is deliberate: replicas honour a
/// tombstone for that long *after* `deleted_at`. By the time 2× TTL has
/// passed, no peer in the network can still hold a stale live record
/// they could resurrect — they would have hit `not_after` long before.
/// Dropping the tombstone after that is safe.
///
/// When a tombstone is deleted, its leaf is also removed from the
/// in-memory Merkle tree. The leaf was advertised during the honour
/// window so anti-entropy could carry the deletion;
/// once the window expires, the leaf disappears from the slice's bitset
/// and a new live record for the same IPK can re-occupy the leaf
/// without diverging from a peer that GC'd theirs first.
///
/// Returns the number of entries evicted (records + tombstones).
///
/// **Caller responsibility:** this is meant to be called from a periodic
/// scheduler (the anti-entropy task), not on a hot RPC path. A
/// full CF scan is `O(records held)` which is small per relay (~300
/// records) but still costs an iterator open.
pub fn evict_expired(dht: &Dht, now_ms: u64) -> usize {
    let tomb_horizon = 2 * PRESENCE_TTL_MS;

    let mut victims: Vec<Vec<u8>> = Vec::new();
    let mut tomb_victim_ipks: Vec<[u8; 32]> = Vec::new();
    for guard in dht.store.presence.iter() {
        let (key, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        match classify_key(&key) {
            KeyKind::Record(_) => {
                if let Ok(rec) = PresenceRecord::deser(&value)
                    && now_ms >= rec.not_after {
                        victims.push(key.to_vec());
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
        if dht.store.presence.remove(&k).is_ok() {
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

/// Sync planning half of the K-set drift migration in `evict_expired`.
///
/// Walks `cf_dht_queue` once and identifies up to `max` queue entries
/// whose recipient `user_ipk` is no longer in this relay's K-closest
/// set. Returns the (key, dispatch) pairs the caller should attempt to
/// migrate via outbound `Forward` RPCs. A successful migration deletes
/// the local entry — but the deletion is the *async* caller's
/// responsibility because it must wait for ≥`FORWARD_K_MIN`
/// confirmations from the new K-closest set first.
///
/// **Why split this from the I/O half**: `evict_expired` is a sync
/// function called from the periodic scheduler. K-set drift migration
/// requires outbound `Forward` RPCs (async, parallel, deadline-bounded
/// — all the machinery in [`super::forward::forward_to_homes`]). The
/// scheduler driver is the right place to compose those: it calls
/// `evict_expired` synchronously, then (when async) `plan_drift_migrations`
/// → spawn a tokio task per candidate → on success the migrator deletes
/// the local queue entry.
///
/// **Per-message K-set check**: one routing-table read per queued entry,
/// bounded by `MAX_MIGRATE_PER_SWEEP` (256) so a pathologically full
/// disk can't stall the sweep.
///
/// **Out of scope**: `messages` keyspace entries are *not*
/// migrated — that CF is the local-fallback safety net owned by the
/// sender's relay. Only `cf_dht_queue` (the home-replica queue) is
/// subject to drift.
pub(crate) fn plan_drift_migrations(
    dht: &Dht, max: usize,
) -> Vec<(MessageKey, DispatchP)> {
    let mut out: Vec<(MessageKey, DispatchP)> = Vec::new();
    if max == 0 {
        return out;
    }

    let self_id = dht.node_id;

    // Cache the per-recipient drift decision so we don't recompute it
    // for every message of the same recipient. The cap is per-sweep,
    // so if a single recipient has 1000 messages and we drifted out
    // of K for them, we'd otherwise issue 1000 routing-table reads
    // for the same answer. Bounded by O(distinct recipients with
    // drifted queues), which in practice is far smaller than the
    // per-sweep cap.
    let mut drifted: std::collections::HashMap<[u8; 32], bool> =
        std::collections::HashMap::new();

    for guard in dht.store.queue.iter() {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        // Validate the key shape and pull the 32-byte recipient prefix.
        if key_bytes.len() != MessageKey::SIZE {
            continue;
        }
        let mut user_ipk = [0u8; 32];
        user_ipk.copy_from_slice(&key_bytes[0..32]);

        let is_drifted = match drifted.get(&user_ipk) {
            Some(b) => *b,
            None => {
                let target_id = NodeId::from_bytes(user_ipk);
                let candidates = dht.routing.read().find_closest(&target_id, K);
                // Permissive sparse-table policy: sparse routing (< K
                // candidates) means we *might* still be K-closest by
                // virtue of nobody else being closer. Treat that as "not
                // drifted" so a freshly-bootstrapped relay doesn't migrate
                // every entry away on its first sweep.
                let drifted_now = if candidates.len() < K {
                    false
                } else {
                    let self_dist = xor32(self_id.as_bytes(), &user_ipk);
                    let kth_dist = xor32(candidates[K - 1].id.as_bytes(), &user_ipk);
                    self_dist > kth_dist
                };
                drifted.insert(user_ipk, drifted_now);
                drifted_now
            }
        };
        if !is_drifted {
            continue;
        }

        let Some(key) = MessageKey::parse(&key_bytes) else {
            continue;
        };
        let Ok(dispatch) = DispatchP::deser(&value) else {
            continue;
        };
        out.push((key, dispatch));
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Delete a single migrated `cf_dht_queue` entry by its composite
/// `MessageKey`. Used by the migration driver after an outbound
/// `Forward` to the new K-closest succeeded. Returns
/// `true` on a successful delete.
///
/// Public-to-the-crate so the `evict_expired` driver in the scheduler
/// can call it from its async migration loop. Lock-free; fjall keyspace
/// handles are internally concurrency-safe for writes from multiple tasks.
pub(crate) fn delete_migrated_entry(dht: &Dht, key: &MessageKey) -> bool {
    dht.store.queue.remove(key.as_bytes()).is_ok()
}

/// Persist a queued [`DispatchP`] into [`CF_DHT_QUEUE`] for the recipient's
/// home-relay queue. Used by:
///
/// - `forward_to_homes` when the sender relay discovers it is itself in
///   the recipient's K-closest set — the self-store short-circuit that
///   mirrors the publish-path's "self in K" optimisation in
///   [`super::publish::publish`].
/// - The home-side `DhtRequest::Forward` handler, to durably enqueue an
///   inbound dispatch for an offline recipient.
///
/// **Cap enforcement.** Mirrors the [`crate::storage::MAX_QUEUED_PER_RECIPIENT`]
/// check in `relay/src/quic/handler/client/events/forward.rs::store_in_rocks`,
/// using a bounded exact `prefix()` scan over the `dht_queue` keyspace.
///
/// Returns:
/// - [`ForwardOutcome::Stored`] on a successful queue write.
/// - [`ForwardOutcome::QueueFull`] when the recipient already has
///   `MAX_QUEUED_PER_RECIPIENT` entries on disk; the dispatch is *not*
///   stored.
/// - [`ForwardOutcome::BadSig`] as a defensive surface for an internal
///   error (postcard serialisation failure, fjall write failure). The
///   on-the-wire semantics of `BadSig` is "we will
///   not accept this dispatch" — surfacing infrastructure failures the
///   same way avoids a silent message-loss path.
///
/// Durability: writes use `WriteOptions::set_sync(true)` so the WAL fsyncs
/// before this returns — same pattern as
/// [`store_record`] and the legacy `store_in_rocks` in
/// `relay/src/quic/handler/client/events/forward.rs`.
///
pub(crate) fn enqueue_for_home(
    dht: &Dht, user_ipk: &[u8; 32], dispatch: &DispatchP, now_ms: u64,
) -> ForwardOutcome {
    // Per-recipient cap. Bounded scan over fjall's exact prefix range: stop
    // as soon as we've confirmed the user is at-or-over the cap so we don't
    // walk a million-entry queue on every Forward.
    let mut count: usize = 0;
    let stop_at = MAX_QUEUED_PER_RECIPIENT.saturating_add(1);
    for guard in dht.store.queue.prefix(user_ipk) {
        // Treat a corrupted iterator as "we can't be sure we're under the
        // cap" — better to reject than silently overrun.
        if guard.key().is_err() {
            return ForwardOutcome::BadSig;
        }
        count += 1;
        if count >= stop_at {
            break;
        }
    }
    if count >= MAX_QUEUED_PER_RECIPIENT {
        dht.metrics.inc_dht_queue_full_rejections();
        return ForwardOutcome::QueueFull;
    }

    let key = MessageKey::new(user_ipk, now_ms, &dispatch.id.0);
    let value = match dispatch.ser() {
        Ok(b) => b,
        Err(_) => return ForwardOutcome::BadSig,
    };

    // fsync before returning so the home relay's "Stored" reply is a durable
    // promise.
    if dht.store.put_sync(&dht.store.queue, key.as_bytes(), &value).is_err() {
        return ForwardOutcome::BadSig;
    }

    dht.metrics.inc_dht_queue_writes();
    ForwardOutcome::Stored
}

/// Look up a tombstone by IPK. Returns `None` if no tombstone is stored.
/// Used by anti-entropy to decide whether a peer's apparent "missing
/// record" is genuinely gone or just stale.
#[allow(dead_code)] // Consumed by the anti-entropy sync RPC handlers.
pub(crate) fn lookup_tombstone(dht: &Dht, user_ipk: &[u8; 32]) -> Option<TombstoneRecord> {
    let key = tombstone_key(user_ipk);
    let bytes = dht.store.presence.get(key).ok().flatten()?;
    TombstoneRecord::deser(&bytes).ok()
}

/// Read up to `max` queued [`DispatchP`]s for `user_ipk` from
/// `cf_dht_queue`, oldest first.
///
/// **Ordering** is naturally chronological: the on-disk `MessageKey`
/// shape `recipient(32) || ts_be(8) || dispatch_id(16)` makes the
/// big-endian timestamp the secondary sort key, so a prefix iterator
/// yields older messages before newer ones for a given recipient. The
/// home-side `QueueFetch` handler returns this Vec verbatim into a
/// `QueueFetchResp`, then the `exhausted` flag is computed by the
/// caller (whether more keys exist past the cap).
///
/// Returns:
/// - `Vec<(MessageKey, DispatchP)>` — bounded by `max` entries.
///   The `MessageKey` is included so the caller can also
///   `delete_queue_entries` on the same iterator pass if it wants to
///   build a one-shot drain instead of a fetch-then-ack cycle (the
///   sticky-home flow does the latter, but the former is a useful
///   primitive for the migration pass).
/// - Empty Vec when there's no queue for `user_ipk` — a soft "nothing to
///   drain" the home-side handler also accepts.
///
/// **Caller's contract**: `parking_lot` lock-discipline applies (no
/// guards held across `await`); this function is sync and takes none.
///
pub(crate) fn lookup_queue_for_user(
    dht: &Dht, user_ipk: &[u8; 32], max: usize,
) -> Vec<(MessageKey, DispatchP)> {
    let mut out: Vec<(MessageKey, DispatchP)> = Vec::new();
    if max == 0 {
        return out;
    }

    // fjall's exact prefix scan + the MessageKey layout (recipient || ts_be
    // || dispatch_id) yields this recipient's queue oldest-first.
    for guard in dht.store.queue.prefix(user_ipk) {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            // Soft-fail on iterator corruption: return what we collected
            // so the caller still drains *something*. The next sweep
            // re-attempts.
            Err(_) => break,
        };
        let Some(key) = MessageKey::parse(&key_bytes) else {
            // Malformed key (length mismatch, etc.) — skip and continue;
            // this is the same defensive policy the legacy local-queue
            // drain uses.
            continue;
        };
        let Ok(dispatch) = DispatchP::deser(&value) else {
            continue;
        };
        out.push((key, dispatch));
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Delete every `cf_dht_queue` entry for `user_ipk` whose
/// `dispatch_id` appears in `dispatch_ids`. Returns the count of
/// successful deletions.
///
/// **Why we iterate-and-filter** rather than computing the full
/// 56-byte `MessageKey` from `(user_ipk, ts, id)` directly: we don't
/// know the original `ts_ms` (it was the `now_ms` at write time, which
/// the requesting relay doesn't have access to). The `dispatch_id`
/// alone identifies the write — but the on-disk key includes the
/// timestamp as a non-prefix component, so the only way to find the
/// matching key is a prefix scan. Bounded by the per-recipient cap
/// (`MAX_QUEUED_PER_RECIPIENT = 1024`), so the worst-case scan is
/// trivial relative to the rest of the RPC's signature-verify cost.
///
/// **Idempotent**: a `dispatch_id` that's already gone (or never
/// existed) contributes 0 to the count. The home-side
/// `QueueFetchAck` handler retries on transient failures, so a partial
/// delete on the first attempt converges over a few rounds.
///
/// Durability: deletions are journal-buffered (no fsync). This is
/// intentional — losing a delete on crash means the dispatch is
/// re-delivered next reconnect (the client dedupes by id), which is
/// strictly better than the alternative cost of fsyncing every per-id
/// delete.
pub(crate) fn delete_queue_entries(
    dht: &Dht, user_ipk: &[u8; 32], dispatch_ids: &[[u8; 16]],
) -> usize {
    if dispatch_ids.is_empty() {
        return 0;
    }

    // Collect target keys first, delete in a second pass (don't mutate the
    // keyspace mid-iteration).
    let target: std::collections::HashSet<[u8; 16]> = dispatch_ids.iter().copied().collect();
    let mut victims: Vec<Vec<u8>> = Vec::new();

    for guard in dht.store.queue.prefix(user_ipk) {
        let key_bytes = match guard.key() {
            Ok(k) => k,
            Err(_) => break,
        };
        // Last 16 bytes of the 56-byte key are the dispatch_id.
        if key_bytes.len() != MessageKey::SIZE {
            continue;
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&key_bytes[40..56]);
        if target.contains(&id) {
            victims.push(key_bytes.to_vec());
        }
    }

    let mut count = 0usize;
    for k in victims {
        if dht.store.queue.remove(&k).is_ok() {
            count += 1;
        }
    }
    count
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

    /// Build a `Dht` instance backed by a fresh tempdir fjall. The
    /// DB lives in `/tmp` so the test doesn't pollute the workspace
    /// (each test gets its own subdir keyed off a counter).
    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-dht-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
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
        let bytes = dht.store.presence.get(rec.user_ipk.0).unwrap();
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
        // Tombstones are honoured for 2 × TTL after their `deleted_at`.
        // Inside that window `evict_expired` must leave them alone.
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
        // Once `deleted_at + 2 × PRESENCE_TTL_MS` has elapsed, the
        // tombstone should be GC'd — both the on-disk entry and its
        // Merkle leaf (so the slice bitset stops advertising it).
        let relay = fresh_signing_key();
        let user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now: u64 = 1_700_000_000_000;
        let tomb = build_tombstone(&user, &relay, 1, now);
        assert_eq!(store_tombstone(&dht, tomb.clone(), now), TombstoneOutcome::Stored);
        // Confirm the Merkle tree now advertises this tombstone
        // (store_tombstone inserts the value-hash into the slice).
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
        // Storing a tombstone must update the Merkle tree (insert with
        // tombstone domain), not remove the leaf entry — so
        // anti-entropy carries deletions.
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

    // -----------------------------------------------------------------
    // Sticky-home — `enqueue_for_home` (cf_dht_queue writes)
    // -----------------------------------------------------------------

    use common::proto::client_rel::DispatchP;
    use common::proto::client_rel::dispatch_sig_message;

    /// Build a fresh, internally-consistent `DispatchP` from `from_user`
    /// to `to_user`. Identical to the `build_dispatch` fixture in
    /// `dht/forward.rs::tests` and `common/src/proto/dht_p2p.rs::tests` —
    /// the duplication is intentional: each test module is self-contained
    /// and a shared fixture would create test-only cross-module deps.
    fn build_dispatch(
        from_user: &SigningKey, to_ipk: &[u8; 32], id: [u8; 16], payload: &[u8],
    ) -> DispatchP {
        let from_ipk: [u8; 32] = from_user.verifying_key().to_bytes();
        let msg = dispatch_sig_message(to_ipk, &from_ipk, &id, payload);
        let sig = from_user.sign(&msg);
        DispatchP {
            to:      (*to_ipk).into(),
            from:    from_ipk.into(),
            id:      id.into(),
            payload: payload.to_vec().into(),
            sig:     sig.to_bytes().into(),
        }
    }

    #[test]
    fn enqueue_for_home_writes_with_correct_key_shape() {
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let id: [u8; 16] = [0xAB; 16];
        let dispatch = build_dispatch(&from_user, &to_ipk, id, b"payload-x");
        let now: u64 = 1_700_000_000_000;

        let outcome = enqueue_for_home(&dht, &to_ipk, &dispatch, now);
        assert_eq!(outcome, ForwardOutcome::Stored);

        // Read back via the prefix iterator the way the home-side
        // QueueFetch handler does. The key shape is
        // `MessageKey { recipient: to_ipk, ts_be: now, id }` per
        // `enqueue_for_home`.
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut values: Vec<Vec<u8>> = Vec::new();
        for guard in dht.store.queue.prefix(to_ipk) {
            let (k, v) = guard.into_inner().expect("iter");
            keys.push(k.to_vec());
            values.push(v.to_vec());
        }
        assert_eq!(keys.len(), 1, "exactly one queued message expected");
        let key = &keys[0];
        // 32-byte recipient + 8-byte big-endian ts + 16-byte id = 56.
        assert_eq!(key.len(), 56, "MessageKey is 56 bytes");
        assert_eq!(&key[0..32], &to_ipk[..]);
        assert_eq!(&key[32..40], &now.to_be_bytes()[..]);
        assert_eq!(&key[40..56], &id[..]);

        // Value is a postcard-encoded DispatchP — round-trip it.
        let decoded = DispatchP::deser(&values[0]).expect("postcard");
        assert_eq!(decoded, dispatch);
    }

    #[test]
    fn enqueue_for_home_returns_queue_full_at_cap() {
        // Fill `cf_dht_queue` for one recipient up to the cap; the next
        // write returns `QueueFull` and does NOT actually write the new
        // entry (we observe by checking the count remained at the cap).
        //
        // We use a deliberately *small* test cap by just exhausting the
        // real `MAX_QUEUED_PER_RECIPIENT = 1024` cap — it's slow but
        // bounded, and the test budget tolerates it (~30 ms in
        // practice). This keeps the test honest: we exercise the
        // production constant rather than mocking it out.
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let now: u64 = 1_700_000_000_000;

        // Direct CF write to bypass the cap (so we can fill quickly).
        // Each entry needs a distinct `(ts, id)` so the keys don't
        // collide. Fill exactly to the cap.
        for i in 0..MAX_QUEUED_PER_RECIPIENT {
            let mut id = [0u8; 16];
            id[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            let key = MessageKey::new(&to_ipk, now + (i as u64), &id);
            // Tiny dummy value — only the count matters for the cap
            // check; the actual deserializability of the value is
            // unrelated to the cap-enforcement path under test.
            dht.store.queue.insert(key.as_bytes(), b"x").expect("put");
        }

        // One more should be rejected with QueueFull.
        let dispatch = build_dispatch(&from_user, &to_ipk, [0xFF; 16], b"overflow");
        let outcome = enqueue_for_home(&dht, &to_ipk, &dispatch, now + 99_999);
        assert_eq!(outcome, ForwardOutcome::QueueFull);

        // And the rejected entry must NOT have been written. Count
        // remains exactly at the cap (the `[0xFF; 16]` id we'd have
        // used absent the cap is not in the queue).
        let mut found_overflow = false;
        for guard in dht.store.queue.prefix(to_ipk) {
            let k = guard.key().expect("iter");
            if k.ends_with(&[0xFF; 16]) {
                found_overflow = true;
                break;
            }
        }
        assert!(!found_overflow, "QueueFull rejection must not write the entry");
    }

    #[test]
    fn enqueue_for_home_accepts_under_cap() {
        // Sanity gate: a small number of writes succeeds and each
        // returns `Stored`. Catches a regression where the cap-check
        // accidentally fires immediately (e.g. an `>=` flipped to `>`
        // with the wrong base).
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let now: u64 = 1_700_000_000_000;

        for i in 0..5u8 {
            let mut id = [0u8; 16];
            id[0] = i;
            let dispatch = build_dispatch(&from_user, &to_ipk, id, b"under-cap");
            let outcome = enqueue_for_home(&dht, &to_ipk, &dispatch, now + i as u64);
            assert_eq!(outcome, ForwardOutcome::Stored, "iter {i}");
        }
    }

    #[test]
    fn enqueue_for_home_does_not_count_other_recipients_against_cap() {
        // Cap is per-recipient. Filling user A's queue must not cause
        // user B to see `QueueFull`. Catches a regression where the
        // `starts_with` filter is dropped and the iterator walks into
        // adjacent users' keyspaces.
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let user_a = fresh_signing_key();
        let user_b = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let ipk_a: [u8; 32] = user_a.verifying_key().to_bytes();
        let ipk_b: [u8; 32] = user_b.verifying_key().to_bytes();
        let now: u64 = 1_700_000_000_000;

        // Fill user A's queue to the cap.
        for i in 0..MAX_QUEUED_PER_RECIPIENT {
            let mut id = [0u8; 16];
            id[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            let key = MessageKey::new(&ipk_a, now + (i as u64), &id);
            dht.store.queue.insert(key.as_bytes(), b"x").expect("put");
        }

        // User B's first write must succeed.
        let dispatch_b = build_dispatch(&from_user, &ipk_b, [1u8; 16], b"hi-B");
        let outcome = enqueue_for_home(&dht, &ipk_b, &dispatch_b, now);
        assert_eq!(outcome, ForwardOutcome::Stored);
    }

    // -----------------------------------------------------------------
    // Sticky-home — `lookup_queue_for_user` + `delete_queue_entries`
    // + `plan_drift_migrations` (the K-set migration planner)
    // -----------------------------------------------------------------

    use common::proto::dht_p2p::NodeDescriptor;

    #[test]
    fn lookup_queue_for_user_returns_chronological_order() {
        // The on-disk key shape is `recipient(32) || ts_be(8) || id(16)`,
        // so the prefix iterator naturally yields oldest-first within a
        // single user. Catches a regression where a future change to
        // the prefix-extractor or key shape breaks ordering.
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        // Insert in non-chronological order (later ts first).
        let dispatch_b = build_dispatch(&from_user, &to_ipk, [2u8; 16], b"second");
        let dispatch_a = build_dispatch(&from_user, &to_ipk, [1u8; 16], b"first");
        enqueue_for_home(&dht, &to_ipk, &dispatch_b, 200);
        enqueue_for_home(&dht, &to_ipk, &dispatch_a, 100);

        let got = lookup_queue_for_user(&dht, &to_ipk, 8);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1.id.0, [1u8; 16]); // ts=100 first
        assert_eq!(got[1].1.id.0, [2u8; 16]); // ts=200 second
    }

    #[test]
    fn lookup_queue_for_user_caps_at_max() {
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();

        for i in 0..10u8 {
            let mut id = [0u8; 16];
            id[0] = i;
            let dispatch = build_dispatch(&from_user, &to_ipk, id, b"x");
            enqueue_for_home(&dht, &to_ipk, &dispatch, 100 + i as u64);
        }

        let got = lookup_queue_for_user(&dht, &to_ipk, 3);
        assert_eq!(got.len(), 3);
        // First three by ts.
        for (i, item) in got.iter().enumerate() {
            assert_eq!(item.1.id.0[0], i as u8);
        }
    }

    #[test]
    fn lookup_queue_for_user_returns_empty_for_unknown_recipient() {
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);
        let unknown_ipk = [0xFFu8; 32];
        let got = lookup_queue_for_user(&dht, &unknown_ipk, 8);
        assert!(got.is_empty());
    }

    #[test]
    fn delete_queue_entries_removes_listed_ids_only() {
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();

        let ids = [[1u8; 16], [2u8; 16], [3u8; 16], [4u8; 16]];
        for &id in &ids {
            let dispatch = build_dispatch(&from_user, &to_ipk, id, b"d");
            enqueue_for_home(&dht, &to_ipk, &dispatch, 100);
        }

        let removed = delete_queue_entries(&dht, &to_ipk, &[ids[1], ids[3]]);
        assert_eq!(removed, 2);

        let remaining = lookup_queue_for_user(&dht, &to_ipk, 8);
        assert_eq!(remaining.len(), 2);
        let remaining_ids: std::collections::HashSet<[u8; 16]> =
            remaining.iter().map(|(_, d)| d.id.0).collect();
        assert!(remaining_ids.contains(&[1u8; 16]));
        assert!(remaining_ids.contains(&[3u8; 16]));
    }

    #[test]
    fn delete_queue_entries_idempotent_for_missing_ids() {
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();

        let dispatch = build_dispatch(&from_user, &to_ipk, [1u8; 16], b"alone");
        enqueue_for_home(&dht, &to_ipk, &dispatch, 100);

        // Delete a never-stored id alongside the one that exists.
        let removed = delete_queue_entries(&dht, &to_ipk, &[[1u8; 16], [2u8; 16]]);
        assert_eq!(removed, 1, "only the present id contributes");

        let remaining = lookup_queue_for_user(&dht, &to_ipk, 8);
        assert!(remaining.is_empty());
    }

    /// `plan_drift_migrations` returns entries whose recipient is no
    /// longer K-closest to self. Set self_id far from the recipient and
    /// install K peers strictly closer; the planner returns the entry.
    #[test]
    fn evict_expired_migrates_queue_when_drifted_out_of_k_closest() {
        // Build a relay whose self_id is `[0xFF; 32]` (far from
        // a recipient at `[0; 32]`); K closer peers force drift.
        let mut self_seed = [0u8; 32];
        self_seed[0] = 0xFF;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        // Install K=3 peers strictly closer to all-zeros target.
        for i in 0..3u8 {
            let mut s = [0u8; 32];
            s[31] = i;
            let id = NodeId::new(s);
            let desc = NodeDescriptor {
                id,
                addr: "127.0.0.1:1".parse().unwrap(),
                pubkey: [0u8; 32].into(),
            };
            dht.routing.write().insert(desc);
        }

        // Pre-populate the queue for an all-zeros recipient.
        let from_user = fresh_signing_key();
        let to_ipk: [u8; 32] = [0u8; 32];
        let dispatch = build_dispatch(&from_user, &to_ipk, [9u8; 16], b"drifted");
        enqueue_for_home(&dht, &to_ipk, &dispatch, 100);

        let migrations = plan_drift_migrations(&dht, 16);
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].1.id.0, [9u8; 16]);
    }

    /// When self is *still* in K-closest, the planner returns nothing
    /// (no migration needed). The default fixture
    /// has an empty routing table → permissive sparse policy → self
    /// counts as K-closest → no entries returned.
    #[test]
    fn evict_expired_no_migration_when_still_owner() {
        let relay = fresh_signing_key();
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();

        let dispatch = build_dispatch(&from_user, &to_ipk, [1u8; 16], b"stay");
        enqueue_for_home(&dht, &to_ipk, &dispatch, 100);

        // Empty routing table → permissive → not drifted.
        let migrations = plan_drift_migrations(&dht, 16);
        assert!(migrations.is_empty());
    }

    /// Cap respected.
    #[test]
    fn evict_expired_caps_migration_at_max_per_sweep() {
        // Force drift for many users, then verify the planner stops
        // at the requested cap.
        let mut self_seed = [0u8; 32];
        self_seed[0] = 0xFF;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        // K closer peers for all-zeros prefix targets.
        for i in 0..3u8 {
            let mut s = [0u8; 32];
            s[31] = i;
            let id = NodeId::new(s);
            let desc = NodeDescriptor {
                id,
                addr: "127.0.0.1:1".parse().unwrap(),
                pubkey: [0u8; 32].into(),
            };
            dht.routing.write().insert(desc);
        }

        // 5 distinct users with leading-zero-byte IPKs so drift
        // applies to all of them.
        let from_user = fresh_signing_key();
        for i in 0..5u8 {
            let mut to_ipk = [0u8; 32];
            to_ipk[31] = 0xA0 | i; // distinct but still "close to 0" target
            let dispatch = build_dispatch(&from_user, &to_ipk, [i; 16], b"cap");
            enqueue_for_home(&dht, &to_ipk, &dispatch, 100);
        }

        let migrations = plan_drift_migrations(&dht, 3);
        assert_eq!(migrations.len(), 3, "cap respected");
    }
}
