//! Offer-scoped permission to serve a completed recipient copy. A grant is
//! accepted only from the authenticated MLS author in the original group;
//! the requesting peer cannot import one or authorize itself with its hash.

use anyhow::{Result, ensure};
use common::proto::mls_wire::AttachmentSharing;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use tokio_util::sync::CancellationToken;

use crate::data::conversation::KIND_GROUP;
use crate::db::messages::MESSAGES_DB;

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

static POLICY: Lazy<Mutex<Policy>> = Lazy::new(|| {
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

/// Freeze the original audience before any dispatch, and reuse the same
/// control ID and plaintext after retry/restart. Missing/expired grants are
/// optional: original-sender delivery still works.
pub(crate) fn for_outgoing(
    conversation: &[u8; 16], group: &[u8; 32], author: &[u8; 32], message_id: [u8; 16],
    file_id: [u8; 32], size: u64, recipients: &[[u8; 32]], roster: &[[u8; 32]],
) -> Result<Option<([u8; 16], AttachmentSharing)>> {
    let now = crate::utils::systime().as_secs();
    let retained_until = super::store::retention_get(&file_id).map_or(0, |r| r.expires_at);
    outgoing_tx(
        &mut MESSAGES_DB.lock(),
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
    let Some(me) = crate::data::identity::Identity::get().map(|i| i.ipk()) else { return Ok(()) };
    let mut conn = MESSAGES_DB.lock();
    receive_tx(&mut conn, conversation, author, me, offer, crate::utils::systime().as_secs())
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

/// A Revise may replace the file before the original Post arrives. Keep this
/// compact revocation independently of both rows. In this first increment,
/// revised posts use the original sender; no old grant silently follows edits.
pub(crate) fn revoke(conversation: &[u8; 16], author: &[u8; 32], target: &[u8; 16]) -> Result<()> {
    MESSAGES_DB.lock().execute(
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
    id: &[u8; 32], file: &[u8; 32], me: &[u8; 32], peer: &[u8; 32],
) -> Option<Grant> {
    let conn = MESSAGES_DB.lock();
    let grant = conn
        .query_row(
            "SELECT * FROM attachment_sharing WHERE grant_id=?1 AND file_id=?2",
            (id.as_slice(), file.as_slice()),
            Grant::from_row,
        )
        .ok()?;
    authorized(&conn, &grant, me, peer, crate::utils::systime().as_secs()).ok()?.then_some(grant)
}

pub(super) fn candidates(
    file: &[u8; 32], original: &[u8; 32], me: &[u8; 32],
) -> Result<Vec<([u8; 32], [u8; 32])>> {
    let conn = MESSAGES_DB.lock();
    let mut stmt = conn.prepare("SELECT * FROM attachment_sharing WHERE file_id=?1 AND author=?2 AND control_id IS NULL AND expires_at>?3 ORDER BY expires_at DESC LIMIT 16")?;
    let grants = stmt.query_map(
        (file.as_slice(), original.as_slice(), crate::utils::systime().as_secs()),
        Grant::from_row,
    )?;
    let mut out = Vec::new();
    for grant in grants {
        let grant = grant?;
        for peer in &grant.offer.recipients {
            if out.iter().any(|(p, _)| p == peer) {
                continue;
            }
            if authorized(&conn, &grant, me, peer, crate::utils::systime().as_secs())? {
                out.push((*peer, grant.id));
            }
        }
    }
    // Rotate bounded attempts between episodes instead of always penalizing
    // the same first members. This nonce stays local and names no wire object.
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

pub(super) fn completed_copy(file: &[u8; 32]) -> Option<super::store::Retention> {
    let copy = super::store::partial_get(file)?;
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
        MESSAGES_DB.lock().execute("DELETE FROM attachment_sharing WHERE expires_at<=?1", [now]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::conversation::Conversation;
    use crate::data::message::Message;

    fn fixture() -> (Connection, Grant) {
        let mut db = crate::db::messages::open_in_memory();
        let conv =
            Conversation::join_group_tx(&db, &[1; 32], &[[1; 32], [2; 32], [3; 32], [4; 32]])
                .unwrap();
        Conversation::bind_group_tx(&db, &conv, &[5; 32]).unwrap();
        let offer = AttachmentSharing {
            message_id: [6; 16],
            file_id: [7; 32],
            size: 8,
            expires_at: 2000,
            recipients: vec![[2; 32], [3; 32]],
        };
        receive_tx(&mut db, conv, [1; 32], [2; 32], offer, 1000).unwrap();
        let grant = db.query_row("SELECT * FROM attachment_sharing", [], Grant::from_row).unwrap();
        (db, grant)
    }

    fn post(db: &Connection, grant: &Grant) {
        Message::save_incoming_tx(
            db,
            grant.conversation,
            grant.author,
            &grant.offer.message_id,
            "file",
            1000,
            None,
        )
        .unwrap();
        db.execute(
            "INSERT INTO message_media (conversation_id,dispatch_id,kind,mime,size,file_id)
            VALUES (?1,?2,?3,'application/octet-stream',?4,?5)",
            params![
                grant.conversation.as_slice(),
                grant.offer.message_id.as_slice(),
                crate::data::media::KIND_ATTACHMENT,
                grant.offer.size,
                grant.offer.file_id.as_slice()
            ],
        )
        .unwrap();
    }

    #[test]
    fn grant_requires_live_original_offer_and_continuing_membership() {
        let (db, grant) = fixture();
        let permitted =
            |db: &Connection, g: &Grant| authorized(db, g, &[2; 32], &[3; 32], 1000).unwrap();
        assert!(!permitted(&db, &grant), "grant before post has no authority");
        post(&db, &grant);
        assert!(permitted(&db, &grant));
        assert!(
            !authorized(&db, &grant, &[2; 32], &[4; 32], 1000).unwrap(),
            "later group member not an original recipient"
        );
        assert!(
            !authorized(&db, &grant, &[2; 32], &[3; 32], 2000).unwrap(),
            "expiry includes exact boundary"
        );
        for key in [[1; 32], [2; 32], [3; 32]] {
            Conversation::deactivate_member_tx(&db, &grant.conversation, &key).unwrap();
            assert!(
                !permitted(&db, &grant),
                "author, provider and requester must all remain active"
            );
            db.execute("UPDATE conversation_members SET active=1 WHERE conversation_id=?1 AND member_ipk=?2",
                (grant.conversation.as_slice(), key.as_slice())).unwrap();
        }
        let mut different = grant.clone();
        different.group = [9; 32];
        assert!(!permitted(&db, &different));
        different = grant.clone();
        different.offer.message_id = [9; 16];
        assert!(!permitted(&db, &different));
        different = grant.clone();
        different.offer.file_id = [9; 32];
        assert!(!permitted(&db, &different));
        different = grant.clone();
        different.author = [9; 32];
        assert!(!permitted(&db, &different));
        different = grant.clone();
        different.offer.size += 1;
        assert!(!permitted(&db, &different));
        db.execute(
            "INSERT INTO attachment_sharing_revocations VALUES (?1,?2,?3)",
            (
                grant.conversation.as_slice(),
                grant.author.as_slice(),
                grant.offer.message_id.as_slice(),
            ),
        )
        .unwrap();
        assert!(!permitted(&db, &grant), "an earlier revision invalidates a later grant/post too");
        db.execute("DELETE FROM attachment_sharing_revocations", []).unwrap();
        db.execute("UPDATE messages SET deleted=1", []).unwrap();
        assert!(!permitted(&db, &grant));
    }

    #[test]
    fn pending_grants_are_bounded_and_do_not_replace_the_original_audience() {
        let (mut db, grant) = fixture();
        let mut changed = grant.offer.clone();
        changed.recipients.push([4; 32]);
        receive_tx(&mut db, grant.conversation, grant.author, [2; 32], changed, 1000).unwrap();
        let saved = db.query_row("SELECT * FROM attachment_sharing", [], Grant::from_row).unwrap();
        assert_eq!(saved.offer, grant.offer);
        for i in 1..MAX_PENDING_PER_AUTHOR {
            let mut offer = grant.offer.clone();
            offer.message_id = [i as u8; 16];
            // Skip the original ID without changing how many distinct slots fill.
            offer.message_id[0] = 0xFF;
            receive_tx(&mut db, grant.conversation, grant.author, [2; 32], offer, 1000).unwrap();
        }
        let mut overflow = grant.offer.clone();
        overflow.message_id = [0xFE; 16];
        assert!(
            receive_tx(&mut db, grant.conversation, grant.author, [2; 32], overflow, 1000).is_err()
        );
        for bad in
            [vec![[2; 32]], vec![[3; 32], [2; 32]], vec![[2; 32], [2; 32]], vec![[1; 32], [2; 32]]]
        {
            let mut offer = grant.offer.clone();
            offer.recipients = bad;
            assert!(validate(&grant.author, &offer, 1000).is_err());
        }
        assert!(
            receive_tx(&mut db, grant.conversation, grant.author, [4; 32], grant.offer, 1000)
                .is_err()
        );
    }

    #[test]
    fn outgoing_snapshot_survives_retry_join_rekey_and_refreshed_retention() {
        let (mut db, grant) = fixture();
        db.execute("DELETE FROM attachment_sharing", []).unwrap();
        post(&db, &grant);
        db.execute("UPDATE messages SET outgoing=1,sender_ipk=NULL", []).unwrap();
        assert!(
            outgoing_tx(
                &mut db,
                &grant.conversation,
                &grant.group,
                &grant.author,
                grant.offer.message_id,
                grant.offer.file_id,
                grant.offer.size,
                &[[2; 32], [3; 32]],
                &[[1; 32], [2; 32], [3; 32]],
                2000,
                1000
            )
            .unwrap()
            .is_none(),
            "old attachment has no original audience proof"
        );
        db.execute("INSERT INTO attachment_sharing_intents SELECT id FROM messages", []).unwrap();
        let roster = [[1; 32], [2; 32], [3; 32], [4; 32]];
        let mut issue = |recipients: &[[u8; 32]], group, expiry, now| {
            outgoing_tx(
                &mut db,
                &grant.conversation,
                &group,
                &grant.author,
                grant.offer.message_id,
                grant.offer.file_id,
                grant.offer.size,
                recipients,
                &roster,
                expiry,
                now,
            )
            .unwrap()
        };
        let first = issue(&[[3; 32], [2; 32]], grant.group, 2000, 1000).unwrap();
        assert_eq!(issue(&roster[1..], grant.group, 3000, 1500), Some(first.clone()));
        assert!(
            issue(&roster[1..], [9; 32], 3000, 1500).is_none(),
            "group replacement cannot transplant grants"
        );
        assert!(
            issue(&roster[1..], grant.group, 4000, 2000).is_none(),
            "same-hash retention refresh cannot extend expired grants"
        );
        assert_eq!(first.1.recipients, vec![[2; 32], [3; 32]]);
        db.execute("DELETE FROM attachment_sharing", []).unwrap();
        assert!(
            outgoing_tx(
                &mut db,
                &grant.conversation,
                &grant.group,
                &grant.author,
                grant.offer.message_id,
                grant.offer.file_id,
                grant.offer.size,
                &roster[1..],
                &roster,
                5000,
                3000
            )
            .unwrap()
            .is_none(),
            "grant GC cannot renew the original audience"
        );
    }

    #[test]
    fn ineligible_first_send_cannot_gain_audience_on_retry() {
        let (mut db, grant) = fixture();
        db.execute("DELETE FROM attachment_sharing", []).unwrap();
        post(&db, &grant);
        db.execute("UPDATE messages SET outgoing=1,sender_ipk=NULL", []).unwrap();
        db.execute("INSERT INTO attachment_sharing_intents SELECT id FROM messages", []).unwrap();
        let roster = [[1; 32], [2; 32], [3; 32]];
        for recipients in [&roster[1..2], &roster[1..]] {
            assert!(
                outgoing_tx(
                    &mut db,
                    &grant.conversation,
                    &grant.group,
                    &grant.author,
                    grant.offer.message_id,
                    grant.offer.file_id,
                    grant.offer.size,
                    recipients,
                    &roster,
                    2000,
                    1000
                )
                .unwrap()
                .is_none()
            );
        }
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM attachment_sharing_intents", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn background_metering_and_rapid_reversal_cancel_old_upload_scopes() {
        let mut policy =
            Policy { foreground: false, wifi: false, cancel: CancellationToken::new() };
        policy.update(Some(true), None);
        assert!(!policy.allowed());
        policy.update(None, Some(true));
        assert!(policy.allowed());
        let first = policy.cancel.clone();
        policy.update(None, Some(true));
        assert!(!first.is_cancelled());
        policy.update(Some(false), None);
        assert!(!policy.allowed());
        policy.update(Some(true), None);
        assert!(policy.allowed());
        assert!(first.is_cancelled(), "a rapid return must not revive the old stream");
        let next = policy.cancel.clone();
        policy.update(None, Some(false));
        tokio::time::timeout(std::time::Duration::from_millis(100), next.cancelled())
            .await
            .unwrap();
        assert!(!policy.allowed());
    }
}
