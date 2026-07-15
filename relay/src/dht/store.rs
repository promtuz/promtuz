//! Home-replica offline-queue persistence over the `dht_queue` keyspace.
//!
//! Owns the sticky-home store-and-forward queue: [`enqueue_for_home`]
//! (writer, per-recipient capped), [`lookup_queue_for_user`] /
//! [`delete_queue_entries`] (the `QueueFetch` / `QueueFetchAck` read +
//! GC paths), and [`plan_drift_migrations`] / [`delete_migrated_entry`]
//! (the K-set drift-migration sweep driven by the scheduler).
//!
//! Values are postcard-encoded [`DispatchP`] keyed by the 56-byte
//! [`MessageKey`] (`recipient(32) || ts_be(8) || dispatch_id(16)`), so a
//! 32-byte prefix scan groups a recipient's queue oldest-first.

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;

use super::Dht;
use super::config::K;
use crate::storage::MAX_QUEUED_PER_RECIPIENT;
use crate::storage::MessageKey;

/// Planning half of the K-set drift migration.
///
/// Walks `cf_dht_queue` once and identifies up to `max` queue entries
/// whose recipient `user_ipk` is no longer in this relay's K-closest
/// set. Returns the (key, dispatch) pairs the caller should attempt to
/// migrate via outbound `Forward` RPCs. A successful migration deletes
/// the local entry — but the deletion is the *async* caller's
/// responsibility because it must wait for ≥`FORWARD_K_MIN`
/// confirmations from the new K-closest set first.
///
/// **Why split the sync planner from the I/O half**: this walk is sync
/// (holds the routing lock briefly per recipient), while the migration
/// itself needs async outbound `Forward` RPCs. The scheduler composes
/// the two: `plan_drift_migrations` → spawn a task per candidate → on
/// success the migrator deletes the local queue entry (see
/// [`super::sync::run_drift_migration_sweep`]).
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
///   the recipient's K-closest set — the self-store short-circuit.
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

    use common::proto::client_rel::dispatch_sig_message;
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
            accepted_at_ms: 1,
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
    fn drift_plan_migrates_queue_when_drifted_out_of_k_closest() {
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
    fn drift_plan_no_migration_when_still_owner() {
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
    fn drift_plan_caps_migration_at_max_per_sweep() {
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
