//! The home queue in the `dht_queue` keyspace: postcard [`DispatchP`] rows under [`MessageKey`], so
//! a recipient prefix scan reads the queue oldest first.

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::pack::MAX_FRAME_BYTES;
use common::quic::id::NodeId;

use super::Dht;
use crate::storage::MessageKey;
use crate::storage::queue::QueueAdmission;
use crate::storage::queued_dispatch;

/// Leaves framing headroom under [`MAX_FRAME_BYTES`]. A bigger batch would make a `QueueFetchResp`
/// the packer refuses, stranding the queue for good.
const QUEUE_BATCH_MAX_BYTES: usize = MAX_FRAME_BYTES - 64 * 1024;

/// Up to `max` queued rows whose recipient no longer has this relay among its homes. The caller
/// deletes a row only after a durable handover.
pub(crate) fn plan_drift_migrations(dht: &Dht, max: usize) -> Vec<(MessageKey, DispatchP)> {
    let mut out: Vec<(MessageKey, DispatchP)> = Vec::new();
    if max == 0 {
        return out;
    }

    let mut drifted: std::collections::HashMap<[u8; 32], bool> = std::collections::HashMap::new();

    for guard in dht.store.queue.iter() {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        let Some(key) = MessageKey::parse(&key_bytes) else {
            continue;
        };
        let user_ipk = key.recipient;
        let is_drifted = *drifted
            .entry(user_ipk)
            .or_insert_with(|| !super::routing::homes(dht, &NodeId::from_bytes(user_ipk)).1);
        if !is_drifted {
            continue;
        }
        let Some(dispatch) = queued_dispatch(&user_ipk, &value) else {
            continue;
        };
        out.push((key, dispatch));
        if out.len() >= max {
            break;
        }
    }
    out
}

pub(crate) fn delete_migrated_entry(dht: &Dht, key: &MessageKey) -> bool {
    dht.store.queue.remove(key.as_bytes()).is_ok()
}

/// `now_ms` is the home's clock, never a wire timestamp: it orders the queue and drives the
/// retention sweep, so an injected dispatch cannot jump the queue or escape the sweep.
pub(crate) fn enqueue_for_home(
    dht: &Dht, user_ipk: &[u8; 32], dispatch: &DispatchP, now_ms: u64,
) -> ForwardOutcome {
    if dispatch.to.0 != *user_ipk {
        return ForwardOutcome::BadSig;
    }
    match dht.store.queue.admit(dispatch, now_ms) {
        Ok(QueueAdmission::Insert | QueueAdmission::AlreadyQueued) => {},
        Ok(QueueAdmission::Full(reason)) => {
            common::trace!(
                "FORWARD: home queue admission limited by {reason} for {}",
                hex::encode(user_ipk)
            );
            return ForwardOutcome::QueueFull;
        },
        Ok(QueueAdmission::IdTakenByOther) | Err(_) => return ForwardOutcome::BadSig,
    }

    ForwardOutcome::Stored
}

/// Also returns whether the batch covers every drainable row. Rows too big to frame or past their
/// sender-declared life are deleted rather than left at the head of the queue.
pub(crate) fn queue_batch_for_user(
    dht: &Dht, user_ipk: &[u8; 32], max: usize, now_ms: u64,
) -> (Vec<(MessageKey, DispatchP)>, bool) {
    let mut out: Vec<(MessageKey, DispatchP)> = Vec::new();
    if max == 0 {
        return (out, false);
    }

    let mut used: usize = 0;
    let mut dead: Vec<Vec<u8>> = Vec::new();
    let mut oversize = 0usize;
    let mut exhausted = true;

    for guard in dht.store.queue.prefix(user_ipk) {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => {
                exhausted = false;
                break;
            },
        };
        let Some(key) = MessageKey::parse(&key_bytes) else {
            continue;
        };
        if value.len() > QUEUE_BATCH_MAX_BYTES {
            oversize += 1;
            dead.push(key_bytes.to_vec());
            continue;
        }
        let Some(dispatch) = queued_dispatch(user_ipk, &value) else {
            continue;
        };
        if dispatch.is_expired(now_ms) {
            dead.push(key_bytes.to_vec());
            continue;
        }
        if out.len() >= max || used.saturating_add(value.len()) > QUEUE_BATCH_MAX_BYTES {
            exhausted = false;
            break;
        }
        used += value.len();
        out.push((key, dispatch));
    }

    let dead: Vec<_> = dead.into_iter().map(Into::into).collect();
    let _ = dht.store.queue.remove_many(&dead);
    if oversize > 0 {
        common::warn!(
            "dht_queue: dropped {oversize} unframeable entr(ies) (> {QUEUE_BATCH_MAX_BYTES} bytes) for {}",
            hex::encode(&user_ipk[..4])
        );
    }

    (out, exhausted)
}

/// Not fsynced: a delete lost in a crash only redelivers, and the client dedupes by id.
pub(crate) fn delete_queue_entries(
    dht: &Dht, user_ipk: &[u8; 32], dispatch_ids: &[[u8; 16]],
) -> usize {
    if dispatch_ids.is_empty() {
        return 0;
    }

    let target: std::collections::HashSet<[u8; 16]> = dispatch_ids.iter().copied().collect();
    let mut keys = Vec::new();
    for id in target {
        let Ok(found) = dht.store.queue.keys_for(user_ipk, &id) else { continue };
        keys.extend(found.into_iter().map(|key| key.as_bytes().into()));
    }
    dht.store.queue.remove_many(&keys).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use common::proto::dht_p2p::MAX_FETCH_QUEUE_BATCH;
    use common::proto::pack::Packer;
    use common::proto::pack::Unpacker;

    use super::*;
    use crate::storage::MAX_QUEUED_PER_RECIPIENT;
    use crate::storage::queue::MAX_QUEUED_PER_SENDER;
    use crate::test_support::dht;
    use crate::test_support::dispatch;
    use crate::test_support::ipk;
    use crate::test_support::key;
    use crate::test_support::put_queued;
    use crate::test_support::queued;

    const NOW: u64 = 1_700_000_000_000;

    fn id(n: usize) -> [u8; 16] {
        let mut id = [0; 16];
        id[..8].copy_from_slice(&(n as u64).to_be_bytes());
        id
    }

    /// On-disk format that survives deploys: recipient, big-endian acceptance time, then id, so
    /// a prefix scan drains one user's queue oldest first.
    #[test]
    fn queue_rows_are_keyed_by_recipient_then_big_endian_time_then_id() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        let to = ipk(2);
        let later = dispatch(&key(3), to, [0xBB; 16], b"later");
        let earlier = dispatch(&key(3), to, [0xAA; 16], b"earlier");
        assert_eq!(enqueue_for_home(&dht, &to, &later, 2_000), ForwardOutcome::Stored);
        assert_eq!(enqueue_for_home(&dht, &to, &earlier, 1_000), ForwardOutcome::Stored);
        let rows: Vec<(Vec<u8>, DispatchP)> = dht
            .store
            .queue
            .prefix(to)
            .map(|row| {
                let (key, value) = row.into_inner().unwrap();
                (key.to_vec(), DispatchP::deser(&value).unwrap())
            })
            .collect();
        let key_of = |at: u64, id: [u8; 16]| [&to[..], &at.to_be_bytes(), &id].concat();
        assert_eq!(
            rows,
            [(key_of(1_000, [0xAA; 16]), earlier), (key_of(2_000, [0xBB; 16]), later)]
        );
    }

    /// Per-recipient cap without writing the overflow, one sender's share below it in both
    /// queues, idempotent retries, refused id squatting, and recipients that never share a cap.
    #[test]
    fn admission_caps_each_recipient_and_sender_and_refuses_a_taken_id() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        let full = ipk(10);
        for n in 0..MAX_QUEUED_PER_RECIPIENT {
            dht.store
                .queue
                .insert(MessageKey::new(&full, NOW + n as u64, &id(n)).as_bytes(), b"x")
                .unwrap();
        }
        let overflow = dispatch(&key(11), full, [0xFF; 16], b"overflow");
        assert_eq!(enqueue_for_home(&dht, &full, &overflow, NOW), ForwardOutcome::QueueFull);
        assert!(!queued(&dht, &full).contains(&[0xFF; 16]), "the overflow is not written");
        let other = ipk(12);
        let elsewhere = dispatch(&key(11), other, [1; 16], b"another recipient");
        assert_eq!(enqueue_for_home(&dht, &other, &elsewhere, NOW), ForwardOutcome::Stored);

        let (to, hog, legit) = (ipk(13), key(14), key(15));
        for n in 0..MAX_QUEUED_PER_SENDER {
            put_queued(&dht, &to, NOW + n as u64, &dispatch(&hog, to, id(n), b"hog"));
        }
        let more = dispatch(&hog, to, [0xEE; 16], b"hog");
        assert_eq!(enqueue_for_home(&dht, &to, &more, NOW), ForwardOutcome::QueueFull);
        let theirs = dispatch(&legit, to, [0xEF; 16], b"legit");
        assert_eq!(enqueue_for_home(&dht, &to, &theirs, NOW), ForwardOutcome::Stored);

        // The relay's local fallback queue holds its rows under the same rule.
        for n in 0..MAX_QUEUED_PER_SENDER {
            let row = dispatch(&hog, to, id(n), b"hog");
            dht.store
                .messages
                .insert(MessageKey::new(&to, NOW + n as u64, &id(n)).as_bytes(), row.ser().unwrap())
                .unwrap();
        }
        assert!(matches!(
            dht.store.messages.admit(&dispatch(&hog, to, [0xEE; 16], b"hog"), NOW).unwrap(),
            QueueAdmission::Full(_)
        ));
        assert!(matches!(
            dht.store.messages.admit(&dispatch(&legit, to, [0xEF; 16], b"legit"), NOW).unwrap(),
            QueueAdmission::Insert
        ));

        let to = ipk(16);
        let first = dispatch(&key(17), to, [7; 16], b"first");
        assert_eq!(enqueue_for_home(&dht, &to, &first, NOW), ForwardOutcome::Stored);
        assert_eq!(
            enqueue_for_home(&dht, &to, &first, NOW + 500),
            ForwardOutcome::Stored,
            "a retry is idempotent"
        );
        let squat = dispatch(&key(18), to, [7; 16], b"squat");
        assert_eq!(enqueue_for_home(&dht, &to, &squat, NOW + 900), ForwardOutcome::BadSig);
        assert_eq!(queued(&dht, &to), [[7; 16]]);
    }

    /// RLY-26: a client retry racing its first attempt is still stored once.
    #[test]
    fn concurrent_retries_of_one_dispatch_are_stored_once() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        for round in 0..20u8 {
            let to = ipk(round);
            let sent = dispatch(&key(100), to, [round; 16], b"retried");
            std::thread::scope(|scope| {
                for at in 0..8 {
                    let (dht, sent) = (&dht, &sent);
                    scope.spawn(move || enqueue_for_home(dht, &to, sent, NOW + at));
                }
            });
            assert_eq!(queued(&dht, &to), [[round; 16]], "round {round}");
        }
    }

    /// A batch always fits a frame, and rows that could never go out, too big to frame or past
    /// their sender's ttl, are deleted rather than left to stall the head of the queue (RLY-02).
    #[test]
    fn batches_fit_a_frame_and_drop_rows_that_could_never_be_delivered() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        let to = ipk(30);
        let big = vec![0x5A; QUEUE_BATCH_MAX_BYTES * 2 / 3];
        for n in 0..3 {
            put_queued(&dht, &to, NOW + n as u64, &dispatch(&key(31), to, id(n), &big));
        }
        let (batch, exhausted) = queue_batch_for_user(&dht, &to, MAX_FETCH_QUEUE_BATCH, NOW);
        assert_eq!((batch.len(), exhausted), (1, false), "a second row would overrun the frame");

        let to = ipk(32);
        let unframable = MessageKey::new(&to, 1, &[0; 16]);
        dht.store.queue.insert(unframable.as_bytes(), vec![0; QUEUE_BATCH_MAX_BYTES + 1]).unwrap();
        for n in 1..=MAX_FETCH_QUEUE_BATCH {
            let mut ring = dispatch(&key(31), to, id(n), b"ring");
            (ring.accepted_at_ms, ring.ttl_ms) = (NOW - 60_000, 40_000);
            put_queued(&dht, &to, 1 + n as u64, &ring);
        }
        put_queued(&dht, &to, NOW, &dispatch(&key(31), to, [0xFF; 16], b"live"));
        let (batch, exhausted) = queue_batch_for_user(&dht, &to, MAX_FETCH_QUEUE_BATCH, NOW);
        assert_eq!(batch.iter().map(|(_, d)| d.id.0).collect::<Vec<_>>(), [[0xFF; 16]]);
        assert!(exhausted);
        assert_eq!(queued(&dht, &to), [[0xFF; 16]], "dead rows are deleted, not skipped");
    }
}
