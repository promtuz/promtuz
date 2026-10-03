//! Offer-scoped permission to serve a completed recipient copy. Only the authenticated MLS author
//! in the original group can grant it; the requesting peer cannot authorize itself.

use anyhow::{Result, ensure};
use common::proto::mls_wire::AttachmentSharing;
use common::utils::now_secs;
use std::sync::LazyLock;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use tokio_util::sync::CancellationToken;

use crate::data::conversation::KIND_GROUP;
use crate::state::Core;
use crate::state::core;

const MAX_LIFETIME: u64 = 7 * 24 * 60 * 60;
const CLOCK_SKEW: u64 = 300;
const MAX_PENDING_PER_AUTHOR: u32 = 64;
const MAX_PENDING: u32 = 1024;
pub(super) const MAX_PROVIDERS: usize = 3;

struct Policy {
    foreground: bool,
    wifi: bool,
    cancel: CancellationToken,
}

impl Policy {
    fn allowed(&self) -> bool {
        self.foreground && self.wifi
    }

    fn update(&mut self, foreground: Option<bool>, wifi: Option<bool>) {
        let before = self.allowed();
        if let Some(value) = foreground {
            self.foreground = value;
        }
        if let Some(value) = wifi {
            self.wifi = value;
        }
        if before != self.allowed() {
            self.cancel.cancel();
            self.cancel = CancellationToken::new();
        }
    }
}

static POLICY: LazyLock<Mutex<Policy>> = LazyLock::new(|| {
    Mutex::new(Policy { foreground: false, wifi: false, cancel: CancellationToken::new() })
});

pub(crate) fn set_foreground(active: bool) {
    POLICY.lock().update(Some(active), None);
}
pub(crate) fn set_network(unmetered_wifi: bool) {
    POLICY.lock().update(None, Some(unmetered_wifi));
}

pub(super) fn serving_scope() -> Option<CancellationToken> {
    let policy = POLICY.lock();
    policy.allowed().then(|| policy.cancel.clone())
}

#[derive(Clone, Debug)]
pub(super) struct Grant {
    pub id: [u8; 32],
    pub conversation: [u8; 16],
    pub group: [u8; 32],
    pub author: [u8; 32],
    pub offer: AttachmentSharing,
    pub control_id: Option<[u8; 16]>,
}

impl Grant {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let bytes: Vec<u8> = row.get("recipients")?;
        let recipients = postcard::from_bytes(&bytes).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Blob, Box::new(e))
        })?;
        Ok(Self {
            id: row.get("grant_id")?,
            conversation: row.get("conversation_id")?,
            group: row.get("group_id")?,
            author: row.get("author")?,
            offer: AttachmentSharing {
                message_id: row.get("message_id")?,
                file_id: row.get("file_id")?,
                size: row.get("size")?,
                expires_at: row.get("expires_at")?,
                recipients,
            },
            control_id: row.get("control_id")?,
        })
    }
}

fn grant_id(group: &[u8; 32], author: &[u8; 32], offer: &AttachmentSharing) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"promtuz-attachment-sharing-v1\0");
    hash.update(group);
    hash.update(author);
    hash.update(&postcard::to_allocvec(offer)?);
    Ok(*hash.finalize().as_bytes())
}

fn validate(author: &[u8; 32], offer: &AttachmentSharing, now: u64) -> Result<()> {
    ensure!(
        offer.expires_at > now && offer.expires_at <= now.saturating_add(MAX_LIFETIME + CLOCK_SKEW),
        "invalid sharing expiry"
    );
    ensure!(offer.size <= i64::MAX as u64, "invalid attachment size");
    ensure!(
        (2..crate::mls::MAX_GROUP_MEMBERS).contains(&offer.recipients.len()),
        "invalid sharing audience size"
    );
    ensure!(
        offer.recipients.windows(2).all(|w| w[0] < w[1]) && !offer.recipients.contains(author),
        "invalid sharing audience"
    );
    Ok(())
}

fn insert(conn: &Connection, grant: &Grant) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO attachment_sharing
         (grant_id,conversation_id,group_id,author,message_id,file_id,size,expires_at,recipients,control_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![grant.id.as_slice(), grant.conversation.as_slice(), grant.group.as_slice(),
            grant.author.as_slice(), grant.offer.message_id.as_slice(), grant.offer.file_id.as_slice(),
            grant.offer.size, grant.offer.expires_at, postcard::to_allocvec(&grant.offer.recipients)?,
            grant.control_id.as_ref().map(|id| id.as_slice())],
    )?;
    Ok(())
}

/// Freezes the audience before any dispatch and reuses the control id and offer across retries.
/// A missing grant is fine: the original sender still serves the file.
pub(crate) fn for_outgoing(
    conversation: &[u8; 16], group: &[u8; 32], author: &[u8; 32], message_id: [u8; 16],
    file_id: [u8; 32], size: u64, recipients: &[[u8; 32]], roster: &[[u8; 32]],
) -> Result<Option<([u8; 16], AttachmentSharing)>> {
    let now = now_secs();
    let retained_until = super::store::retention_get(&file_id).map_or(0, |r| r.expires_at);
    outgoing_tx(
        &mut core().db.messages().lock(),
        conversation,
        group,
        author,
        message_id,
        file_id,
        size,
        recipients,
        roster,
        retained_until,
        now,
    )
}

fn outgoing_tx(
    conn: &mut Connection, conversation: &[u8; 16], group: &[u8; 32], author: &[u8; 32],
    message_id: [u8; 16], file_id: [u8; 32], size: u64, recipients: &[[u8; 32]],
    roster: &[[u8; 32]], retained_until: u64, now: u64,
) -> Result<Option<([u8; 16], AttachmentSharing)>> {
    let tx = conn.transaction()?;
    let existing = tx.query_row(
        "SELECT * FROM attachment_sharing WHERE conversation_id=?1 AND author=?2 AND message_id=?3",
        (conversation.as_slice(), author.as_slice(), message_id.as_slice()), Grant::from_row,
    ).optional()?;
    if let Some(saved) = existing {
        return Ok(saved
            .control_id
            .filter(|_| {
                saved.group == *group
                    && saved.offer.file_id == file_id
                    && saved.offer.size == size
                    && saved.offer.expires_at > now
            })
            .map(|id| (id, saved.offer)));
    }
    // Consume on the first attempt even if the audience is too small or the
    // source is unavailable. A later join/retry must not reinterpret history.
    let first = tx.execute(
        "DELETE FROM attachment_sharing_intents WHERE message_id IN
        (SELECT id FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=1)",
        (conversation.as_slice(), message_id.as_slice()),
    )?;
    if first == 0 {
        return Ok(None);
    }
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages m JOIN conversations c ON c.id=m.conversation_id
         JOIN message_media mm ON mm.conversation_id=m.conversation_id AND mm.dispatch_id=m.dispatch_id
         WHERE m.conversation_id=?1 AND m.dispatch_id=?2 AND m.outgoing=1 AND m.deleted=0 AND m.edited=0
           AND c.kind=?3 AND c.mls_group_id=?4 AND mm.file_id=?5 AND mm.size=?6)",
        params![conversation.as_slice(), message_id.as_slice(), KIND_GROUP, group.as_slice(), file_id.as_slice(), size],
        |r| r.get(0),
    )?;
    if !live || !roster.contains(author) {
        tx.commit()?;
        return Ok(None);
    }
    let mut audience: Vec<_> =
        recipients.iter().copied().filter(|p| p != author && roster.contains(p)).collect();
    audience.sort_unstable();
    audience.dedup();
    if audience.len() < 2 {
        tx.commit()?;
        return Ok(None);
    }
    let offer = AttachmentSharing {
        message_id,
        file_id,
        size,
        expires_at: retained_until.min(now.saturating_add(MAX_LIFETIME)),
        recipients: audience,
    };
    if validate(author, &offer, now).is_err() {
        tx.commit()?;
        return Ok(None);
    }
    let control_id = crate::data::message::next_dispatch_id();
    let grant = Grant {
        id: grant_id(group, author, &offer)?,
        conversation: *conversation,
        group: *group,
        author: *author,
        offer: offer.clone(),
        control_id: Some(control_id),
    };
    insert(&tx, &grant)?;
    tx.commit()?;
    Ok(Some((control_id, offer)))
}

/// Only call after MLS authentication, with the leaf author. A pending grant
/// has no authority until the independently authenticated Post is present.
pub(crate) fn receive(
    conversation: [u8; 16], author: [u8; 32], offer: AttachmentSharing,
) -> Result<()> {
    let Some(me) = crate::data::identity::Identity::local_ipk() else { return Ok(()) };
    let mut conn = core().db.messages().lock();
    receive_tx(&mut conn, conversation, author, me, offer, now_secs())
}

fn receive_tx(
    conn: &mut Connection, conversation: [u8; 16], author: [u8; 32], me: [u8; 32],
    offer: AttachmentSharing, now: u64,
) -> Result<()> {
    validate(&author, &offer, now)?;
    ensure!(offer.recipients.contains(&me), "sharing grant does not name us");
    let tx = conn.transaction()?;
    let group: [u8; 32] = tx.query_row(
        "SELECT mls_group_id FROM conversations WHERE id=?1 AND kind=?2",
        (conversation.as_slice(), KIND_GROUP),
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM attachment_sharing WHERE expires_at<=?1", [now])?;
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND sender_ipk=?2 AND dispatch_id=?3 AND outgoing=0)",
        (conversation.as_slice(), author.as_slice(), offer.message_id.as_slice()), |r| r.get(0),
    )?;
    if !present {
        let (total, mine): (u32, u32) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(s.conversation_id=?1 AND s.author=?2),0) FROM attachment_sharing s
             WHERE s.control_id IS NULL AND NOT EXISTS(SELECT 1 FROM messages m
               WHERE m.conversation_id=s.conversation_id AND m.sender_ipk=s.author AND m.dispatch_id=s.message_id)",
            (conversation.as_slice(), author.as_slice()), |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            total < MAX_PENDING && mine < MAX_PENDING_PER_AUTHOR,
            "pending sharing grants full"
        );
    }
    let grant = Grant {
        id: grant_id(&group, &author, &offer)?,
        conversation,
        group,
        author,
        offer,
        control_id: None,
    };
    insert(&tx, &grant)?;
    tx.commit()?;
    Ok(())
}

/// A Revise may replace the file before the original Post arrives, so the revocation is kept apart
/// from both rows. No old grant follows an edit.
pub(crate) fn revoke(conversation: &[u8; 16], author: &[u8; 32], target: &[u8; 16]) -> Result<()> {
    core().db.messages().lock().execute(
        "INSERT OR IGNORE INTO attachment_sharing_revocations (conversation_id,author,message_id) VALUES (?1,?2,?3)",
        (conversation.as_slice(), author.as_slice(), target.as_slice()),
    )?;
    Ok(())
}

fn authorized(
    conn: &Connection, grant: &Grant, me: &[u8; 32], peer: &[u8; 32], now: u64,
) -> Result<bool> {
    if grant.offer.expires_at <= now
        || me == peer
        || !grant.offer.recipients.contains(me)
        || !grant.offer.recipients.contains(peer)
    {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversations c
         JOIN messages m ON m.conversation_id=c.id
         JOIN message_media mm ON mm.conversation_id=m.conversation_id AND mm.dispatch_id=m.dispatch_id
         WHERE c.id=?1 AND c.mls_group_id=?2 AND c.kind=?3
           AND m.sender_ipk=?4 AND m.dispatch_id=?5 AND m.outgoing=0 AND m.deleted=0 AND m.edited=0
           AND mm.file_id=?6 AND mm.size=?7 AND mm.kind=?8
           AND NOT EXISTS(SELECT 1 FROM attachment_sharing_revocations r WHERE r.conversation_id=c.id AND r.author=?4 AND r.message_id=?5)
           AND EXISTS(SELECT 1 FROM conversation_members WHERE conversation_id=c.id AND member_ipk=?4 AND active=1)
           AND EXISTS(SELECT 1 FROM conversation_members WHERE conversation_id=c.id AND member_ipk=?9 AND active=1)
           AND EXISTS(SELECT 1 FROM conversation_members WHERE conversation_id=c.id AND member_ipk=?10 AND active=1))",
        params![grant.conversation.as_slice(), grant.group.as_slice(), KIND_GROUP, grant.author.as_slice(),
            grant.offer.message_id.as_slice(), grant.offer.file_id.as_slice(), grant.offer.size,
            crate::data::media::KIND_ATTACHMENT, me.as_slice(), peer.as_slice()], |r| r.get(0),
    )?)
}

pub(super) fn permitted(
    c: &Core, id: &[u8; 32], file: &[u8; 32], me: &[u8; 32], peer: &[u8; 32],
) -> Option<Grant> {
    let conn = c.db.messages().lock();
    let grant = conn
        .query_row(
            "SELECT * FROM attachment_sharing WHERE grant_id=?1 AND file_id=?2",
            (id.as_slice(), file.as_slice()),
            Grant::from_row,
        )
        .ok()?;
    authorized(&conn, &grant, me, peer, now_secs()).ok()?.then_some(grant)
}

pub(super) fn candidates(
    c: &Core, file: &[u8; 32], original: &[u8; 32], me: &[u8; 32],
) -> Result<Vec<([u8; 32], [u8; 32])>> {
    let conn = c.db.messages().lock();
    let mut stmt = conn.prepare("SELECT * FROM attachment_sharing WHERE file_id=?1 AND author=?2 AND control_id IS NULL AND expires_at>?3 ORDER BY expires_at DESC LIMIT 16")?;
    let grants = stmt.query_map(
        (file.as_slice(), original.as_slice(), now_secs()),
        Grant::from_row,
    )?;
    let mut out = Vec::new();
    for grant in grants {
        let grant = grant?;
        for peer in &grant.offer.recipients {
            if out.iter().any(|(p, _)| p == peer) {
                continue;
            }
            if authorized(&conn, &grant, me, peer, now_secs())? {
                out.push((*peer, grant.id));
            }
        }
    }
    // A local nonce rotates which members get the bounded attempts.
    let nonce = crate::data::message::next_dispatch_id();
    out.sort_unstable_by_key(|(peer, _)| {
        let mut h = blake3::Hasher::new();
        h.update(&nonce);
        h.update(peer);
        *h.finalize().as_bytes()
    });
    out.truncate(MAX_PROVIDERS);
    Ok(out)
}

pub(super) fn completed_copy(c: &Core, file: &[u8; 32]) -> Option<super::store::Retention> {
    let copy = super::store::partial_get_tx(&c.db.transfers().lock(), file)?;
    if !copy.is_complete() {
        return None;
    }
    Some(super::store::Retention {
        path: copy.path,
        size: copy.total,
        chunk_size: copy.chunk_size,
        manifest: copy.manifest?,
        expires_at: u64::MAX,
    })
}

pub(super) fn gc(now: u64) {
    // Expiry removes metadata only. Consumed outgoing intents prevent retries
    // from issuing fresh grants after this collection or a retention refresh.
    let _ =
        core().db.messages().lock().execute("DELETE FROM attachment_sharing WHERE expires_at<=?1", [now]);
}

#[cfg(test)]
mod tests {
    use common::utils::now_secs;

    use super::super::ranges;
    use super::super::store;
    use super::super::v2::ErrorCode;
    use super::super::v2::Frame;
    use super::super::v2::ReadPhase;
    use super::super::v2::{
        self,
    };
    use super::super::wire;
    use super::*;
    use crate::data::conversation::Conversation;
    use crate::data::message::Message;
    use crate::mls::policy::ROLE_MEMBER;
    use crate::p2p::PeerLink;
    use crate::p2p::protocol::offered_alpns;
    use crate::test_support::transfer::Device;
    use crate::test_support::transfer::attachment;
    use crate::test_support::transfer::device;
    use crate::test_support::transfer::identity;
    use crate::test_support::transfer::linked;
    use crate::test_support::transfer::manifest;

    /// A four-member group where member 2 holds author 1's grant to share file 7 with member 3.
    fn fixture() -> (Connection, Grant) {
        let mut db = crate::test_support::data::open(crate::db::messages::migrate);
        let members = [[1; 32], [2; 32], [3; 32], [4; 32]];
        let conv = Conversation::join_group_tx(&db, &[1; 32], &members).unwrap();
        Conversation::bind_group_tx(&db, &conv, &[5; 32]).unwrap();
        let offer = AttachmentSharing {
            message_id: [6; 16],
            file_id:    [7; 32],
            size:       8,
            expires_at: 2000,
            recipients: vec![[2; 32], [3; 32]],
        };
        receive_tx(&mut db, conv, [1; 32], [2; 32], offer, 1000).unwrap();
        let grant = db.query_row("SELECT * FROM attachment_sharing", [], Grant::from_row).unwrap();
        (db, grant)
    }

    fn post(db: &Connection, grant: &Grant) {
        let (conv, id) = (grant.conversation, grant.offer.message_id);
        Message::save_incoming_tx(db, conv, grant.author, &id, "file", 1000, None).unwrap();
        let row = attachment(grant.offer.file_id, grant.offer.size);
        crate::data::media::save_tx(db, &conv, &id, &row).unwrap();
    }

    #[test]
    fn grant_requires_live_original_offer_and_continuing_membership() {
        let (db, grant) = fixture();
        let permitted =
            |db: &Connection, g: &Grant| authorized(db, g, &[2; 32], &[3; 32], 1000).unwrap();
        assert!(!permitted(&db, &grant), "a grant before its post has no authority");
        post(&db, &grant);
        assert!(permitted(&db, &grant));
        assert!(
            !authorized(&db, &grant, &[2; 32], &[4; 32], 1000).unwrap(),
            "a later member was not sent it"
        );
        assert!(
            !authorized(&db, &grant, &[2; 32], &[3; 32], 2000).unwrap(),
            "expiry includes its boundary"
        );
        for key in [[1; 32], [2; 32], [3; 32]] {
            Conversation::deactivate_member_tx(&db, &grant.conversation, &key).unwrap();
            assert!(!permitted(&db, &grant), "author, provider and requester all stay members");
            Conversation::put_member(&db, &grant.conversation, &key, ROLE_MEMBER).unwrap();
        }
        let changes: [fn(&mut Grant); 5] = [
            |g| g.group = [9; 32],
            |g| g.offer.message_id = [9; 16],
            |g| g.offer.file_id = [9; 32],
            |g| g.author = [9; 32],
            |g| g.offer.size += 1,
        ];
        for change in changes {
            let mut other = grant.clone();
            change(&mut other);
            assert!(!permitted(&db, &other), "{other:?}");
        }
        let (conv, author, id) = (grant.conversation, grant.author, grant.offer.message_id);
        let revoke = "INSERT INTO attachment_sharing_revocations VALUES (?1, ?2, ?3)";
        db.execute(revoke, params![conv.as_slice(), author.as_slice(), id.as_slice()]).unwrap();
        assert!(!permitted(&db, &grant), "an earlier revision revokes it");
        db.execute("DELETE FROM attachment_sharing_revocations", []).unwrap();
        db.execute("UPDATE messages SET deleted = 1", []).unwrap();
        assert!(!permitted(&db, &grant), "deleting the post revokes it");
    }

    fn issue(
        db: &mut Connection, g: &Grant, recipients: &[[u8; 32]], roster: &[[u8; 32]],
        group: [u8; 32], retained_until: u64, now: u64,
    ) -> Option<([u8; 16], AttachmentSharing)> {
        let (file, size) = (g.offer.file_id, g.offer.size);
        outgoing_tx(
            db,
            &g.conversation,
            &group,
            &g.author,
            g.offer.message_id,
            file,
            size,
            recipients,
            roster,
            retained_until,
            now,
        )
        .unwrap()
    }

    #[test]
    fn outgoing_snapshot_survives_retry_join_rekey_and_refreshed_retention() {
        let (mut db, grant) = fixture();
        db.execute("DELETE FROM attachment_sharing", []).unwrap();
        post(&db, &grant);
        db.execute("UPDATE messages SET outgoing = 1, sender_ipk = NULL", []).unwrap();
        let all = [[1; 32], [2; 32], [3; 32], [4; 32]];
        let intent = "INSERT INTO attachment_sharing_intents SELECT id FROM messages";
        assert!(
            issue(&mut db, &grant, &all[1..3], &all[..3], grant.group, 2000, 1000).is_none(),
            "a message sent before sharing has no recorded audience"
        );

        db.execute(intent, []).unwrap();
        assert!(issue(&mut db, &grant, &all[1..2], &all[..3], grant.group, 2000, 1000).is_none());
        assert!(
            issue(&mut db, &grant, &all[1..3], &all[..3], grant.group, 2000, 1000).is_none(),
            "an audience too small at first send never grows on retry"
        );

        db.execute(intent, []).unwrap();
        let first = issue(&mut db, &grant, &[[3; 32], [2; 32]], &all, grant.group, 2000, 1000);
        let first = first.unwrap();
        assert_eq!(first.1.recipients, vec![[2; 32], [3; 32]]);
        assert_eq!(
            issue(&mut db, &grant, &all[1..], &all, grant.group, 3000, 1500),
            Some(first),
            "a retry after a join keeps the original audience"
        );
        assert!(
            issue(&mut db, &grant, &all[1..], &all, [9; 32], 3000, 1500).is_none(),
            "a replacement group cannot take it over"
        );
        assert!(
            issue(&mut db, &grant, &all[1..], &all, grant.group, 4000, 2000).is_none(),
            "refreshed retention cannot extend an expired grant"
        );
        db.execute("DELETE FROM attachment_sharing", []).unwrap();
        assert!(
            issue(&mut db, &grant, &all[1..], &all, grant.group, 5000, 3000).is_none(),
            "collecting grants does not renew the audience"
        );
    }

    /// `me`'s copy of author `author`'s post of `file` to `recipients`, with the grant it carried.
    fn shared(
        dev: &Device, me: [u8; 32], author: [u8; 32], recipients: &[[u8; 32]],
        file: &wire::Manifest, expires_at: u64,
    ) -> [u8; 32] {
        let mut db = dev.core.db.messages().lock();
        let members = [&[author][..], recipients].concat();
        let conv = Conversation::join_group_tx(&db, &author, &members).unwrap();
        Conversation::bind_group_tx(&db, &conv, &[0xe2; 32]).unwrap();
        Message::save_incoming_tx(&db, conv, author, &[0xe3; 16], "file", 1000, None).unwrap();
        let row = attachment(file.file_id(), file.total_size);
        crate::data::media::save_tx(&db, &conv, &[0xe3; 16], &row).unwrap();
        let offer = AttachmentSharing {
            message_id: [0xe3; 16],
            file_id: file.file_id(),
            size: file.total_size,
            expires_at,
            recipients: recipients.to_vec(),
        };
        receive_tx(&mut db, conv, author, me, offer, now_secs()).unwrap();
        db.query_row("SELECT grant_id FROM attachment_sharing", [], |r| r.get(0)).unwrap()
    }

    async fn describe_shared(
        link: &PeerLink, local: &wire::Auth, file_id: [u8; 32], grant: [u8; 32],
    ) -> Frame {
        let (mut s, mut r, _) = super::super::pull::open_v2_request(link, local).await.unwrap();
        v2::write_frame(&mut s, &Frame::DescribeShared { file_id, grant }).await.unwrap();
        s.finish().unwrap();
        v2::read_frame_for(&mut r, ReadPhase::Manifest).await.unwrap()
    }

    #[tokio::test]
    async fn recipient_copies_fall_back_past_old_and_damaged_helpers_and_stop_when_backgrounded() {
        set_foreground(true);
        set_network(true);
        let author = identity(210).ipk;
        let (receiver, old, bad, good) =
            (identity(211), identity(212), identity(213), identity(214));
        let mut recipients = vec![receiver.ipk, old.ipk, bad.ipk, good.ipk];
        recipients.sort_unstable();
        let bytes: Vec<u8> =
            (0..6 * wire::CHUNK_SIZE + 31).map(|i| (i / wire::CHUNK_SIZE) as u8 + 37).collect();
        let file = manifest(&bytes, wire::CHUNK_SIZE);
        let (fid, chunks) = (file.file_id(), file.chunks.len() as u32);
        let expires = now_secs() + 3600;

        let old_link = linked(&old, &receiver, offered_alpns(), offered_alpns()).await;
        let (server, local) = (old_link.server.clone(), old.clone());
        let old_peer = tokio::spawn(async move {
            let (mut s, mut r) = server.accept_stream().await.unwrap();
            super::super::auth::exchange(&server.conn, &mut s, &mut r, server.ipk, &local)
                .await
                .unwrap();
            assert!(matches!(
                v2::read_frame_for(&mut r, ReadPhase::Hello).await,
                Ok(Frame::Hello(_))
            ));
            let released = v2::Hello { supported: 1, required: 1, ..v2::Hello::local() };
            v2::write_frame(&mut s, &Frame::Hello(released)).await.unwrap();
            r.read(&mut [0]).await
        });
        let mut links = vec![(old.ipk, old_link)];
        let mut helpers = Vec::new();
        for helper in [&bad, &good] {
            let dev = device();
            shared(&dev, helper.ipk, author, &recipients, &file, expires);
            let mut copy = bytes.clone();
            if helper.ipk == bad.ipk {
                copy[2 * wire::CHUNK_SIZE] ^= 1;
            }
            let path = dev.dir.path().join("copy");
            std::fs::write(&path, copy).unwrap();
            let done = store::Partial {
                file_id:    fid,
                source_ipk: author,
                total:      file.total_size,
                chunk_size: file.chunk_size,
                manifest:   Some(postcard::to_allocvec(&file).unwrap()),
                have:       chunks,
                state:      store::DONE,
                path:       path.display().to_string(),
                updated_at: 1000,
            };
            let lease = store::receiver_lease(&dev.core.db, fid);
            store::partial_put_live_tx(&dev.core.db.transfers().lock(), &done, &lease).unwrap();
            drop(lease);
            let link = linked(helper, &receiver, offered_alpns(), offered_alpns()).await;
            tokio::spawn(super::super::serve::serve_streams(
                dev.core,
                link.server.clone(),
                helper.clone(),
            ));
            links.push((helper.ipk, link));
            helpers.push(dev);
        }

        let r = device();
        let grant = shared(&r, receiver.ipk, author, &recipients, &file, expires);
        let lease = store::receiver_lease(&r.core.db, fid);
        let mut partial = ranges::Receiver::open_async(
            r.core,
            fid,
            author,
            file.clone(),
            file.total_size,
            &lease,
        )
        .await
        .unwrap();
        for i in [0, 5] {
            let range = i * wire::CHUNK_SIZE..(i + 1) * wire::CHUNK_SIZE;
            partial.commit(i as u32, &bytes[range], &lease).unwrap();
        }
        drop(partial);
        let mut attempts = 0;
        let candidates = vec![(old.ipk, grant), (bad.ipk, grant), (good.ipk, grant)];
        let helped = super::super::pull::try_helpers(
            r.core,
            fid,
            file.total_size,
            &receiver,
            &lease,
            candidates,
            |peer| {
                attempts += 1;
                if attempts == 3 {
                    assert_eq!(r.verified(&fid), 3, "the damaged helper's good chunk is kept");
                }
                let link = links.iter().find(|(ipk, _)| *ipk == peer).unwrap().1.client.clone();
                std::future::ready(Ok(link))
            },
        )
        .await
        .unwrap();
        assert!(helped);
        assert_eq!(attempts, 3);
        assert!(
            !matches!(old_peer.await.unwrap(), Ok(Some(_))),
            "a released peer gets no new request"
        );
        let copy = r.partial(&fid).unwrap();
        assert!(copy.is_complete());
        assert_eq!(std::fs::read(&copy.path).unwrap(), bytes);

        let good_link = &links[2].1.client;
        let (mut s, mut rx, _) = super::super::pull::open_v2_request(good_link, &receiver).await.unwrap();
        let ranges = vec![ranges::ChunkRange { start: 0, end: chunks }];
        v2::write_frame(&mut s, &Frame::PullShared { file_id: fid, grant, ranges }).await.unwrap();
        s.finish().unwrap();
        let first = v2::read_frame_for(&mut rx, ReadPhase::Chunk).await.unwrap();
        assert!(matches!(first, Frame::Chunk { index: 0, .. }));
        set_foreground(false);
        let mut received = 1;
        while let Ok(Frame::Chunk { .. }) = v2::read_frame_for(&mut rx, ReadPhase::Chunk).await {
            received += 1;
        }
        assert!(received < chunks, "backgrounding stops an upload already streaming");
        let refused = describe_shared(good_link, &receiver, fid, grant).await;
        assert_eq!(refused, Frame::Error(ErrorCode::Unavailable));
        set_foreground(true);
        let served = describe_shared(good_link, &receiver, fid, grant).await;
        assert!(matches!(served, Frame::Manifest(_)));
    }
}
