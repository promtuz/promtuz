//! The home queue in the `dht_queue` keyspace: postcard [`DispatchP`] rows under [`MessageKey`], so
//! a recipient prefix scan reads the queue oldest first.

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::pack::MAX_FRAME_BYTES;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;

use super::Dht;
use crate::storage::MAX_QUEUED_PER_RECIPIENT;
use crate::storage::MessageKey;

/// One sender's share of a recipient's [`MAX_QUEUED_PER_RECIPIENT`] slots. Kept above
/// `MAX_FETCH_QUEUE_BATCH` so a single busy conversation still pages normally.
const MAX_QUEUED_PER_SENDER: usize = 128;

/// Ceiling on queued bytes deserialized to attribute rows to their sender
/// during one admission scan.
const SENDER_SCAN_BYTE_BUDGET: usize = 8 * 1024 * 1024;

pub(crate) enum QueueAdmission {
    Insert,
    AlreadyQueued,
    IdTakenByOther,
    Full,
    ScanFailed,
}

/// Per-recipient cap, per-sender share and one row per id. Past [`SENDER_SCAN_BYTE_BUDGET`] rows go
/// unattributed and only the per-recipient cap binds.
pub(crate) fn admit_to_queue(
    ks: &fjall::Keyspace, recipient: &[u8; 32], dispatch_id: &[u8; 16], sender: &[u8; 32],
    sender_of: impl Fn(&[u8]) -> Option<[u8; 32]>,
) -> QueueAdmission {
    let mut total: usize = 0;
    let mut from_sender: usize = 0;
    let mut attributed_bytes: usize = 0;
    let stop_at = MAX_QUEUED_PER_RECIPIENT.saturating_add(1);
    for guard in ks.prefix(recipient) {
        // A failed scan cannot prove the queue is under its cap, so it rejects.
        let Ok((key_bytes, value)) = guard.into_inner() else {
            return QueueAdmission::ScanFailed;
        };
        if key_bytes.len() != MessageKey::SIZE {
            continue;
        }
        let attributable = attributed_bytes < SENDER_SCAN_BYTE_BUDGET;
        let same_sender = attributable && sender_of(&value).is_some_and(|f| f == *sender);
        if attributable {
            attributed_bytes += value.len();
        }
        if key_bytes[40..56] == *dispatch_id {
            return if same_sender {
                QueueAdmission::AlreadyQueued
            } else {
                QueueAdmission::IdTakenByOther
            };
        }
        total += 1;
        if same_sender {
            from_sender += 1;
            if from_sender >= MAX_QUEUED_PER_SENDER {
                break;
            }
        }
        if total >= stop_at {
            break;
        }
    }
    if total >= MAX_QUEUED_PER_RECIPIENT || from_sender >= MAX_QUEUED_PER_SENDER {
        return QueueAdmission::Full;
    }
    QueueAdmission::Insert
}

/// Leaves framing headroom under [`MAX_FRAME_BYTES`]. A bigger batch would make a `QueueFetchResp`
/// the packer refuses, stranding the queue for good.
const QUEUE_BATCH_MAX_BYTES: usize = MAX_FRAME_BYTES - 64 * 1024;

/// Up to `max` queued rows whose recipient no longer has this relay among its homes. The caller
/// deletes a row only after a durable handover.
pub(crate) fn plan_drift_migrations(
    dht: &Dht, max: usize,
) -> Vec<(MessageKey, DispatchP)> {
    let mut out: Vec<(MessageKey, DispatchP)> = Vec::new();
    if max == 0 {
        return out;
    }

    let mut drifted: std::collections::HashMap<[u8; 32], bool> =
        std::collections::HashMap::new();

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

pub(crate) fn delete_migrated_entry(dht: &Dht, key: &MessageKey) -> bool {
    dht.store.queue.remove(key.as_bytes()).is_ok()
}

/// `now_ms` is the home's clock, never a wire timestamp: it orders the queue and drives the
/// retention sweep, so an injected dispatch cannot jump the queue or escape the sweep.
pub(crate) fn enqueue_for_home(
    dht: &Dht, user_ipk: &[u8; 32], dispatch: &DispatchP, now_ms: u64,
) -> ForwardOutcome {
    let _admission = dht.store.admission(user_ipk);
    match admit_to_queue(&dht.store.queue, user_ipk, &dispatch.id.0, &dispatch.from.0, |v| {
        DispatchP::deser(v).ok().map(|d| d.from.0)
    }) {
        QueueAdmission::Insert => {},
        QueueAdmission::AlreadyQueued => return ForwardOutcome::Stored,
        QueueAdmission::IdTakenByOther | QueueAdmission::ScanFailed => {
            return ForwardOutcome::BadSig;
        },
        QueueAdmission::Full => {
            return ForwardOutcome::QueueFull;
        },
    }

    let key = MessageKey::new(user_ipk, now_ms, &dispatch.id.0);
    let value = match dispatch.ser() {
        Ok(b) => b,
        Err(_) => return ForwardOutcome::BadSig,
    };

    // Queues the group-commit fsync. `Stored` is a durable promise, so the caller awaits
    // `Store::persist_barrier` before putting it on the wire.
    if dht.store.put_sync(&dht.store.queue, key.as_bytes(), &value).is_err() {
        return ForwardOutcome::BadSig;
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
        let Ok(dispatch) = DispatchP::deser(&value) else {
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

    for key in &dead {
        let _ = dht.store.queue.remove(key);
    }
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
    let mut victims: Vec<Vec<u8>> = Vec::new();

    for guard in dht.store.queue.prefix(user_ipk) {
        let key_bytes = match guard.key() {
            Ok(k) => k,
            Err(_) => break,
        };
        if MessageKey::parse(&key_bytes).is_some_and(|key| target.contains(&key.id)) {
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

#[cfg(test)]
mod tests {
    use common::proto::client_rel::DeliverP;
    use common::proto::dht_p2p::MAX_FETCH_QUEUE_BATCH;

    use super::*;
    use crate::quic::handler::client::events::forward::dispatch_to_deliver;
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

        // The relay's local fallback queue holds `DeliverP` rows under the same rule.
        let sender_of = |value: &[u8]| DeliverP::deser(value).ok().map(|d| d.from.0);
        for n in 0..MAX_QUEUED_PER_SENDER {
            let row = dispatch_to_deliver(dispatch(&hog, to, id(n), b"hog"));
            dht.store
                .messages
                .insert(MessageKey::new(&to, n as u64, &id(n)).as_bytes(), row.ser().unwrap())
                .unwrap();
        }
        let hog_ipk = hog.verifying_key().to_bytes();
        let legit_ipk = legit.verifying_key().to_bytes();
        assert!(matches!(
            admit_to_queue(&dht.store.messages, &to, &[0xEE; 16], &hog_ipk, sender_of),
            QueueAdmission::Full
        ));
        assert!(matches!(
            admit_to_queue(&dht.store.messages, &to, &[0xEF; 16], &legit_ipk, sender_of),
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
