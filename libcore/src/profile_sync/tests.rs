use std::collections::VecDeque;

use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use rusqlite::OptionalExtension;

use super::*;
use crate::test_support::data::open;
use crate::test_support::data::with_failing_trigger;

const PICTURE: &[u8] = b"\0\0\0\x0cftypavif";

fn photo(revision: u64) -> Snapshot {
    Snapshot::Avatar(AvatarUpdate { revision, avif: Some(PICTURE.to_vec()) })
}

fn details(revision: u64, name: &str) -> Snapshot {
    Snapshot::Details(ProfileUpdate {
        revision,
        name: name.into(),
        bio: "Bio".into(),
        card: vec![],
    })
}

/// One device: its messages.db and its own current revision of one part.
struct Node {
    db:  Connection,
    own: ([u8; 32], Snapshot),
}

impl Node {
    fn new(key: u8, own: Snapshot) -> Self {
        Self { db: open(crate::db::messages::migrate), own: ([key; 32], own) }
    }

    /// Through the wire codec, as a peer's control arrives.
    fn receive(&self, peer: &Self, payload: AppPayload) -> Vec<AppPayload> {
        let payload = AppPayload::deser(&payload.ser().unwrap()).unwrap();
        receive_tx(&self.db, Some(&self.own), &peer.own.0, payload).unwrap().1
    }

    fn ack(&self, peer: &Self) -> Option<u64> {
        let sql = format!(
            "SELECT revision FROM {} WHERE owner_ipk = ?1 AND peer_ipk = ?2",
            self.own.1.part().ack_table()
        );
        self.db
            .query_row(&sql, (self.own.0.as_slice(), peer.own.0.as_slice()), |r| r.get(0))
            .optional()
            .unwrap()
            .flatten()
    }

    fn retries(&self, peer: &Self) -> bool {
        let part = self.own.1.part();
        should_retry(part, &self.db, &self.own.0, &peer.own.0, self.own.1.revision()).unwrap()
    }
}

/// One reconnect probe from `a`, every reply delivered until both sides are quiet. Returns how
/// many pictures or details crossed.
fn reconnect(a: &Node, b: &Node) -> usize {
    let part = a.own.1.part();
    let probe = part.sync(known_revision(part, &a.db, &b.own.0).unwrap(), false);
    let mut queue = VecDeque::from([(false, probe)]);
    let (mut count, mut carried) = (0, 0);
    while let Some((to_a, payload)) = queue.pop_front() {
        count += 1;
        assert!(count <= 6, "profile controls must not form a reply loop");
        if matches!(payload, AppPayload::Avatar { .. } | AppPayload::ProfileDetails { .. }) {
            carried += 1;
        }
        let replies = if to_a { a.receive(b, payload) } else { b.receive(a, payload) };
        queue.extend(replies.into_iter().map(|p| (!to_a, p)));
    }
    carried
}

/// Both first pictures were swallowed by older apps, then a removal was lost too.
#[test]
fn reconnects_repair_lost_pictures_and_a_late_upload_acks_the_removal() {
    let mut a = Node::new(1, photo(10));
    let b = Node::new(2, photo(20));
    assert_eq!(reconnect(&a, &b), 2, "one reconnect repairs both directions");
    assert_eq!(peer_avatar::get_tx(&b.db, &a.own.0).as_deref(), Some(PICTURE));
    assert_eq!(peer_avatar::get_tx(&a.db, &b.own.0).as_deref(), Some(PICTURE));
    assert_eq!((a.ack(&b), b.ack(&a)), (Some(10), Some(20)));
    assert_eq!(reconnect(&a, &b), 0, "a converged pair exchanges no pictures");

    a.own.1 = Snapshot::Avatar(AvatarUpdate { revision: 11, avif: None });
    assert!(a.retries(&b));
    assert_eq!(reconnect(&a, &b), 1);
    assert_eq!(peer_avatar::get_tx(&b.db, &a.own.0), None);
    assert_eq!(a.ack(&b), Some(11));
    let late = b.receive(&a, photo(10).into_payload());
    assert!(
        matches!(late.as_slice(), [AppPayload::AvatarAck { revision: 11 }]),
        "the tombstone is acked"
    );
    assert_eq!(peer_avatar::get_tx(&b.db, &a.own.0), None);
}

#[test]
fn acks_are_scoped_and_cannot_confirm_a_future_revision() {
    let a = Node::new(1, photo(10));
    let b = Node::new(2, photo(20));
    let stranger = Node::new(3, photo(30));
    // Support is established, but the photo and its ACK are lost.
    a.receive(&b, AppPayload::AvatarSync { known_revision: None, reply: true });
    assert!(a.retries(&b));
    assert!(!a.retries(&stranger), "a peer never heard from is probed only on reconnect");
    b.receive(&a, photo(10).into_payload());
    assert_eq!(a.ack(&b), None);
    assert_eq!(reconnect(&a, &b), 1, "only B's photo was missing; the sync repairs A's lost ACK");
    assert_eq!(a.ack(&b), Some(10));

    a.receive(&b, AppPayload::AvatarAck { revision: 9 });
    a.receive(&b, AppPayload::AvatarAck { revision: u64::MAX });
    assert_eq!(a.ack(&b), Some(10), "neither a stale nor a future revision moves the ACK");
    a.receive(&stranger, AppPayload::AvatarAck { revision: 10 });
    assert_eq!(a.ack(&stranger), Some(10));
    assert!(
        should_retry(Part::Avatar, &a.db, &a.own.0, &b.own.0, 11).unwrap(),
        "an old ACK confirms nothing new"
    );
    assert!(
        !should_retry(Part::Avatar, &a.db, &[99; 32], &b.own.0, 11).unwrap(),
        "another identity inherits no ACK"
    );
    assert!(
        !should_retry(Part::Details, &a.db, &a.own.0, &b.own.0, 10).unwrap(),
        "an avatar exchange says nothing about details"
    );
}

#[test]
fn a_restored_peer_reopens_retry_even_with_an_older_photo() {
    let a = Node::new(1, photo(10));
    let b = Node::new(2, photo(20));
    reconnect(&a, &b);
    let resend = a.receive(&b, AppPayload::AvatarSync { known_revision: Some(9), reply: true });
    assert!(matches!(resend.as_slice(), [AppPayload::Avatar { revision: 10, .. }]));
    assert_eq!(a.ack(&b), Some(9));
    assert!(a.retries(&b), "the earlier ACK of 10 no longer counts");
    a.receive(&b, AppPayload::AvatarSync { known_revision: None, reply: true });
    assert_eq!(a.ack(&b), None);
}

#[test]
fn details_and_avatar_receipts_move_independently() {
    let a = Node::new(1, details(10, "Alice"));
    let b = Node::new(2, details(20, "Bob"));
    assert_eq!(reconnect(&a, &b), 2);
    assert_eq!(reconnect(&a, &b), 0);
    let own_avatar = (a.own.0, photo(10));
    receive_tx(&a.db, Some(&own_avatar), &b.own.0, AppPayload::AvatarAck { revision: 10 }).unwrap();
    // B restores and lost A's details; it reconnects first.
    b.db.execute("DELETE FROM peer_profiles WHERE ipk = ?1", [a.own.0.as_slice()]).unwrap();
    assert_eq!(reconnect(&b, &a), 1);
    assert_eq!(a.ack(&b), Some(10));
    let name: String = b
        .db
        .query_row("SELECT name FROM peer_profiles WHERE ipk = ?1", [a.own.0.as_slice()], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(name, "Alice");
    assert!(!should_retry(Part::Avatar, &a.db, &a.own.0, &b.own.0, 10).unwrap());

    receive_tx(
        &a.db,
        Some(&a.own),
        &b.own.0,
        AppPayload::ProfileDetailsSync { known_revision: None, reply: true },
    )
    .unwrap();
    assert!(a.retries(&b));
    assert!(
        !should_retry(Part::Avatar, &a.db, &a.own.0, &b.own.0, 10).unwrap(),
        "the avatar receipt stands"
    );
    a.receive(&b, AppPayload::ProfileDetailsAck { revision: u64::MAX });
    assert!(a.retries(&b));
}

/// Nothing is sent before commit, so storage that fails never produces an ACK.
#[test]
fn only_a_committed_write_is_acknowledged() {
    let a = Node::new(1, photo(10));
    let mut b = Node::new(2, photo(20));
    let peer = a.own.0;

    // The insert succeeds and the commit fails on a deferred key.
    b.db.execute_batch(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER fail_commit AFTER INSERT ON peer_avatars BEGIN INSERT INTO child VALUES (1); END;",
    )
    .unwrap();
    assert!(receive_tx(&b.db, Some(&b.own), &peer, photo(10).into_payload()).is_err());
    assert_eq!(known_revision(Part::Avatar, &b.db, &peer).unwrap(), None);
    b.db.execute_batch("DROP TRIGGER fail_commit;").unwrap();

    let refused = AppPayload::Avatar { revision: 11, avif: Some(b"not an image".to_vec()) };
    assert!(
        receive_tx(&b.db, Some(&b.own), &peer, refused).is_err(),
        "invalid bytes are not acked"
    );
    assert_eq!(known_revision(Part::Avatar, &b.db, &peer).unwrap(), None);
    let acked = b.receive(&a, photo(10).into_payload());
    assert!(matches!(acked.as_slice(), [AppPayload::AvatarAck { revision: 10 }]));

    let profile = |revision, name: &str, bio: &str| AppPayload::ProfileDetails {
        revision,
        name: name.into(),
        bio: bio.into(),
        card: vec![],
    };
    let (_, ack) = receive_tx(&b.db, None, &peer, profile(20, "New", "")).unwrap();
    assert!(matches!(ack.as_slice(), [AppPayload::ProfileDetailsAck { revision: 20 }]));
    let (changed, ack) = receive_tx(&b.db, None, &peer, profile(10, "Old", "Removed")).unwrap();
    assert!(!changed, "stale details cannot restore a removed bio");
    assert!(matches!(ack.as_slice(), [AppPayload::ProfileDetailsAck { revision: 20 }]));
    with_failing_trigger(&mut b.db, "peer_profiles", "UPDATE", |db| {
        assert!(receive_tx(db, None, &peer, profile(30, "Failed", "x")).is_err());
    });
    assert_eq!(known_revision(Part::Details, &b.db, &peer).unwrap(), Some(20));
}
