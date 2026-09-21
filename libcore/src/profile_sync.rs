//! Repair profile state independently of message delivery. Relay acceptance
//! cannot tell us whether the receiving app understood and stored an avatar.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use common::proto::mls_wire::AppPayload;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::peer_avatar::{self, AvatarUpdate};
use crate::db::messages::MESSAGES_DB;

const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
// Suppress reconnect storms and overlapping relay lifetimes. A reconnect still
// probes a peer even when its earlier ACK was current: it may have lost state.
const PROBE_COOLDOWN: Duration = Duration::from_secs(60);
type PeerKey = ([u8; 32], [u8; 32]);
static PROBES: Lazy<Mutex<HashMap<PeerKey, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn known_revision(conn: &Connection, peer: &[u8; 32]) -> Result<Option<u64>> {
    Ok(conn
        .query_row("SELECT revision FROM peer_avatars WHERE ipk = ?1", [peer.as_slice()], |r| {
            r.get(0)
        })
        .optional()?)
}

fn acknowledge(
    conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], revision: u64, current: u64,
) -> Result<()> {
    // A peer may acknowledge an older snapshot, but cannot pre-ack future edits.
    if revision > current {
        return Ok(());
    }
    let revision = i64::try_from(revision)?;
    conn.execute(
        "INSERT INTO avatar_acks (owner_ipk, peer_ipk, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT(owner_ipk, peer_ipk) DO UPDATE SET revision = excluded.revision
         WHERE excluded.revision > COALESCE(avatar_acks.revision, -1)",
        (owner.as_slice(), peer.as_slice(), revision),
    )?;
    Ok(())
}

/// Apply a control and produce responses in one transaction. Nothing is sent
/// before commit; failed storage must never generate a successful ACK.
fn receive_tx(
    conn: &Connection, own: Option<&([u8; 32], AvatarUpdate)>, peer: &[u8; 32], payload: AppPayload,
) -> Result<(bool, Vec<AppPayload>)> {
    let tx = conn.unchecked_transaction()?;
    let mut changed = false;
    let mut responses = Vec::new();
    match payload {
        AppPayload::Avatar { revision, avif } => {
            changed = peer_avatar::apply_tx(&tx, peer, &AvatarUpdate { revision, avif })?;
            // A delayed upload after a removal acknowledges the retained newer
            // tombstone, not the stale incoming bytes. Duplicates also re-ACK.
            if let Some(revision) = known_revision(&tx, peer)? {
                responses.push(AppPayload::AvatarAck { revision });
            }
        },
        AppPayload::AvatarAck { revision } => {
            if let Some((owner, update)) = own {
                acknowledge(&tx, owner, peer, revision, update.revision)?;
            }
        },
        AppPayload::AvatarSync { known_revision: received, reply } => {
            let Some((owner, update)) = own else { bail!("identity unavailable") };
            // The exchange proves support even if they have never seen a photo.
            tx.execute(
                "INSERT OR IGNORE INTO avatar_acks (owner_ipk, peer_ipk) VALUES (?1, ?2)",
                (owner.as_slice(), peer.as_slice()),
            )?;
            if received != Some(update.revision) {
                // A restored peer may have no copy OR an older copy. Either
                // claim invalidates our previous evidence of their latest copy.
                tx.execute(
                    "UPDATE avatar_acks SET revision = NULL WHERE owner_ipk = ?1 AND peer_ipk = ?2",
                    (owner.as_slice(), peer.as_slice()),
                )?;
            }
            if let Some(revision) = received {
                acknowledge(&tx, owner, peer, revision, update.revision)?;
            }
            if !reply {
                responses.push(AppPayload::AvatarSync {
                    known_revision: known_revision(&tx, peer)?,
                    reply: true,
                });
            }
            if received != Some(update.revision) {
                responses.push(update.clone().into_payload());
            }
        },
        _ => bail!("not a profile control"),
    }
    tx.commit()?;
    Ok((changed, responses))
}

/// Shared by live delivery and messages released from MLS epoch catch-up.
/// `peer` is the authenticated MLS author, never a key supplied in the payload.
pub(crate) fn receive(conversation: [u8; 16], peer: [u8; 32], payload: AppPayload) {
    // Never nest the identity lock inside the messages lock.
    let own = Identity::get().map(|i| (i.ipk(), i.avatar_update()));
    let result = receive_tx(&MESSAGES_DB.lock(), own.as_ref(), &peer, payload);
    match result {
        Ok((changed, responses)) => {
            if changed {
                peer_avatar::notify_changed();
            }
            if responses.is_empty() {
                return;
            }
            crate::RUNTIME.spawn(async move {
                for response in responses {
                    if let Err(e) =
                        crate::messaging::send_control_to(conversation, response, peer).await
                    {
                        log::debug!("PROFILE: reconciliation response failed: {e}");
                    }
                }
            });
        },
        Err(e) => log::warn!("PROFILE: could not persist profile control: {e}"),
    }
}

fn should_retry(
    conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], current: u64,
) -> Result<bool> {
    // Unknown/old clients are probed on reconnect, not repeatedly disturbed by
    // new control variants. Once support is established, retry missing ACKs.
    let ack: Option<Option<u64>> = conn
        .query_row(
            "SELECT revision FROM avatar_acks WHERE owner_ipk = ?1 AND peer_ipk = ?2",
            (owner.as_slice(), peer.as_slice()),
            |r| r.get(0),
        )
        .optional()?;
    Ok(ack.is_some_and(|revision| revision != Some(current)))
}

fn claim_probe(probes: &mut HashMap<PeerKey, Instant>, key: PeerKey, now: Instant) -> bool {
    if probes.get(&key).is_some_and(|last| now.duration_since(*last) < PROBE_COOLDOWN) {
        return false;
    }
    probes.insert(key, now);
    true
}

async fn reconcile(all: bool) -> Result<()> {
    let Some(identity) = Identity::get() else { return Ok(()) };
    let owner = identity.ipk();
    let current = identity.avatar_update().revision;
    let mut peers = HashMap::new();
    // One shared active chat per peer is enough, even if we share many groups.
    for chat in Conversation::list().into_iter().filter(|c| c.mls_group_id.is_some()) {
        let members = Conversation::members(&chat.id);
        if !members.iter().any(|m| m.active && m.member_ipk == owner) {
            continue;
        }
        for member in members.into_iter().filter(|m| m.active && m.member_ipk != owner) {
            peers.entry(member.member_ipk).or_insert(chat.id);
        }
    }
    let now = Instant::now();
    PROBES.lock().retain(|(who, peer), _| *who == owner && peers.contains_key(peer));
    for (peer, conversation) in peers {
        let known = {
            let conn = MESSAGES_DB.lock();
            if !all && !should_retry(&conn, &owner, &peer, current)? {
                continue;
            }
            known_revision(&conn, &peer)?
        };
        if !claim_probe(&mut PROBES.lock(), (owner, peer), now) {
            continue;
        }
        if let Err(e) = crate::messaging::send_control_to(
            conversation,
            AppPayload::AvatarSync { known_revision: known, reply: false },
            peer,
        )
        .await
        {
            log::debug!("PROFILE: reconciliation probe failed: {e}");
        }
    }
    Ok(())
}

pub(crate) async fn run(cancel: CancellationToken) {
    let mut all = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = reconcile(all) => {
                if let Err(e) = result { log::warn!("PROFILE: reconciliation failed: {e}"); }
            },
        }
        all = false;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;
    use common::proto::pack::{Packer, Unpacker};
    use std::collections::VecDeque;

    fn photo(revision: u64) -> AvatarUpdate {
        AvatarUpdate { revision, avif: Some(b"\0\0\0\x0cftypavif".to_vec()) }
    }

    struct Node {
        db: Connection,
        own: ([u8; 32], AvatarUpdate),
    }
    impl Node {
        fn new(key: u8, picture: AvatarUpdate) -> Self {
            Self { db: open_in_memory(), own: ([key; 32], picture) }
        }
        fn receive(&self, peer: &Self, payload: AppPayload) -> Vec<AppPayload> {
            // Exercise the actual codec too; a skipped old-client payload never
            // reaches this handler, just as after decrypting an unknown variant.
            let payload = AppPayload::deser(&payload.ser().unwrap()).unwrap();
            receive_tx(&self.db, Some(&self.own), &peer.own.0, payload).unwrap().1
        }
        fn ack(&self, peer: &Self) -> Option<u64> {
            self.db
                .query_row(
                    "SELECT revision FROM avatar_acks WHERE owner_ipk = ?1 AND peer_ipk = ?2",
                    (self.own.0.as_slice(), peer.own.0.as_slice()),
                    |r| r.get(0),
                )
                .optional()
                .unwrap()
                .flatten()
        }
    }

    fn reconnect(a: &Node, b: &Node) -> usize {
        // A single reconnect repairs BOTH directions. No echo loop, and no
        // pictures are sent when the two sides already agree.
        let mut queue = VecDeque::from([(
            false,
            AppPayload::AvatarSync {
                known_revision: known_revision(&a.db, &b.own.0).unwrap(),
                reply: false,
            },
        )]);
        let mut count = 0;
        let mut pictures = 0;
        while let Some((to_a, payload)) = queue.pop_front() {
            count += 1;
            assert!(count <= 6, "profile controls must not form a reply loop");
            if matches!(payload, AppPayload::Avatar { .. }) {
                pictures += 1;
            }
            let replies = if to_a { a.receive(b, payload) } else { b.receive(a, payload) };
            queue.extend(replies.into_iter().map(|p| (!to_a, p)));
        }
        pictures
    }

    #[test]
    fn upgrade_recovers_dropped_photo_then_missed_removal_without_resending_current_images() {
        let mut a = Node::new(1, photo(10));
        let b = Node::new(2, photo(20));
        // Both initial Avatar messages were consumed by older apps. There is
        // no ciphertext to replay and no peer-avatar row at either end.
        assert_eq!(reconnect(&a, &b), 2);
        assert_eq!(peer_avatar::get_tx(&b.db, &a.own.0), a.own.1.avif);
        assert_eq!(peer_avatar::get_tx(&a.db, &b.own.0), b.own.1.avif);
        assert_eq!(a.ack(&b), Some(10));
        assert_eq!(b.ack(&a), Some(20));
        assert_eq!(reconnect(&a, &b), 0);

        // A removal also gets lost. Only the remover reconnects this time.
        a.own.1 = AvatarUpdate { revision: 11, avif: None };
        assert!(should_retry(&a.db, &a.own.0, &b.own.0, 11).unwrap());
        assert_eq!(reconnect(&a, &b), 1);
        assert_eq!(peer_avatar::get_tx(&b.db, &a.own.0), None);
        assert_eq!(known_revision(&b.db, &a.own.0).unwrap(), Some(11));
        assert_eq!(a.ack(&b), Some(11));
        // A late upload cannot restore the deleted photo and ACKs revision 11.
        let responses = b.receive(&a, photo(10).into_payload());
        assert!(matches!(responses.as_slice(), [AppPayload::AvatarAck { revision: 11 }]));
    }

    #[test]
    fn missing_ack_is_repaired_and_receipts_are_scoped_and_cannot_preack_future_edits() {
        let a = Node::new(1, photo(10));
        let b = Node::new(2, photo(20));
        let stranger = Node::new(3, photo(30));
        // Establish support but drop the photo and its ACK.
        a.receive(&b, AppPayload::AvatarSync { known_revision: None, reply: true });
        assert!(should_retry(&a.db, &a.own.0, &b.own.0, 10).unwrap());
        assert!(
            !should_retry(&a.db, &a.own.0, &stranger.own.0, 10).unwrap(),
            "unknown clients only probed on reconnect"
        );
        // Receiver stores the photo; drop only its ACK this time.
        b.receive(&a, photo(10).into_payload());
        assert_eq!(a.ack(&b), None);
        assert_eq!(
            reconnect(&a, &b),
            1,
            "only B's photo is missing; its sync also repairs A's lost ACK"
        );
        assert_eq!(a.ack(&b), Some(10));
        a.receive(&b, AppPayload::AvatarAck { revision: 9 });
        a.receive(&b, AppPayload::AvatarAck { revision: u64::MAX });
        assert_eq!(a.ack(&b), Some(10));
        a.receive(&stranger, AppPayload::AvatarAck { revision: 10 });
        assert_eq!(a.ack(&stranger), Some(10));
        assert!(
            should_retry(&a.db, &a.own.0, &b.own.0, 11).unwrap(),
            "an old ACK cannot confirm a new revision"
        );
        assert!(
            !should_retry(&a.db, &[99; 32], &b.own.0, 11).unwrap(),
            "another identity inherits no capability or ACK"
        );
    }

    #[test]
    fn restored_peer_invalidates_a_previous_ack_even_when_it_still_has_an_older_photo() {
        let a = Node::new(1, photo(10));
        let b = Node::new(2, photo(20));
        reconnect(&a, &b);
        // Simulate B reporting a restored snapshot and a failed resend. Pending
        // must remain visible to the timer despite the earlier revision-10 ACK.
        let response =
            a.receive(&b, AppPayload::AvatarSync { known_revision: Some(9), reply: true });
        assert!(matches!(response.as_slice(), [AppPayload::Avatar { revision: 10, .. }]));
        assert_eq!(a.ack(&b), Some(9));
        assert!(should_retry(&a.db, &a.own.0, &b.own.0, 10).unwrap());
        a.receive(&b, AppPayload::AvatarSync { known_revision: None, reply: true });
        assert_eq!(a.ack(&b), None);
    }

    #[test]
    fn failed_commit_never_acknowledges_or_partially_stores_a_photo() {
        let a = Node::new(1, photo(10));
        let b = Node::new(2, photo(20));
        b.db.execute_batch(
            "PRAGMA foreign_keys = ON;
            CREATE TABLE parent (id INTEGER PRIMARY KEY);
            CREATE TABLE child (id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
            CREATE TRIGGER fail_commit AFTER INSERT ON peer_avatars BEGIN
                INSERT INTO child VALUES (123);
            END;",
        )
        .unwrap();
        assert!(receive_tx(&b.db, Some(&b.own), &a.own.0, photo(10).into_payload()).is_err());
        assert_eq!(known_revision(&b.db, &a.own.0).unwrap(), None);
        b.db.execute_batch("DROP TRIGGER fail_commit;").unwrap();
        assert!(matches!(
            b.receive(&a, photo(10).into_payload()).as_slice(),
            [AppPayload::AvatarAck { revision: 10 }]
        ));
    }

    #[test]
    fn reconnect_storms_share_one_probe_but_allow_later_reconciliation() {
        use std::sync::{Arc, Barrier};
        let probes = Arc::new(Mutex::new(HashMap::new()));
        let barrier = Arc::new(Barrier::new(8));
        let now = Instant::now();
        let key = ([1; 32], [2; 32]);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let probes = probes.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_probe(&mut probes.lock(), key, now) as usize
                })
            })
            .collect();
        assert_eq!(workers.into_iter().map(|w| w.join().unwrap()).sum::<usize>(), 1);
        assert!(!claim_probe(
            &mut probes.lock(),
            key,
            now + PROBE_COOLDOWN - Duration::from_millis(1)
        ));
        assert!(claim_probe(&mut probes.lock(), key, now + PROBE_COOLDOWN));
    }
}
