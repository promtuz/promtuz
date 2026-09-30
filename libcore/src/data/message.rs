use anyhow::Result;
use parking_lot::Mutex;
use ulid::Ulid;

use crate::db::messages::MESSAGES_DB;
use crate::db::messages::MessageRow;
use crate::utils::systime;

/// Message status constants. Failed is a separate send outcome, not progress
/// beyond sent. Delivery and read evidence take precedence over send outcomes.
pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SENT: u8 = 1;
pub const STATUS_FAILED: u8 = 2;
pub const STATUS_DELIVERED: u8 = 3;
pub const STATUS_READ: u8 = 4;

/// Strictly-monotonic 16-byte dispatch id. `Uuid::now_v7()` is only
/// millisecond-monotonic (random tail), so two sends in the same ms don't
/// order by send time — which would let a "delivered up to X" watermark
/// mark a not-yet-delivered sibling. Clamp each mint to strictly greater
/// than the last. Serialized on one device by this lock (cheap).
// ponytail: process-local monotonic; a burst can push the id's ts bits a
// hair ahead of wall-clock — harmless, it's a sortable token, not a clock.
static LAST_DISPATCH_ID: Mutex<u128> = Mutex::new(0);

pub fn next_dispatch_id() -> [u8; 16] {
    let mut last = LAST_DISPATCH_ID.lock();
    let mut v = u128::from_be_bytes(uuid::Uuid::now_v7().into_bytes());
    if v <= *last {
        v = *last + 1;
    }
    *last = v;
    v.to_be_bytes()
}

#[derive(Debug, Clone)]
pub struct Message {
    pub inner: MessageRow,
}

impl MessageRow {
    /// Who wrote this. Outgoing rows store no sender — we are the only
    /// possibility — so they resolve to `me`.
    pub fn sender(&self, me: &[u8; 32]) -> [u8; 32] {
        self.sender_ipk
            .as_ref()
            .and_then(|v| v.as_slice().try_into().ok())
            .unwrap_or(*me)
    }
}

impl Message {
    /// Save an outgoing message (status = pending until relay confirms).
    /// `reply_to` is the quoted message's dispatch_id, if this is a reply.
    pub fn save_outgoing(
        conversation_id: [u8; 16], content: &str, reply_to: Option<[u8; 16]>,
    ) -> Result<Self> {
        let me = super::identity::Identity::get().map(|i| i.ipk());
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let row = Self::save_outgoing_tx(&tx, conversation_id, content, reply_to, me)?;
        tx.commit()?;
        Ok(row)
    }

    /// Transaction-scoped [`Self::save_outgoing`]: same insert against a
    /// caller-supplied connection, so an outgoing media message persists its
    /// caption row and its `message_media` row in ONE transaction — a
    /// media-write failure rolls the caption back instead of leaving a
    /// caption-only orphan the send path can never repair.
    pub fn save_outgoing_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], content: &str,
        reply_to: Option<[u8; 16]>, me: Option<[u8; 32]>,
    ) -> Result<Self> {
        let id = Ulid::new();
        let timestamp = systime().as_secs();
        let dispatch_id = next_dispatch_id();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status, dispatch_id, reply_to) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
            (&id.to_string(), conversation_id.as_slice(), content, timestamp, STATUS_PENDING, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
        )?;

        // Snapshot failure must roll the message back, not freeze an empty
        // audience. Resolve identity before locking this DB at the call site.
        let recipients = conn
            .prepare(
                "SELECT member_ipk FROM conversation_members
            WHERE conversation_id=?1 AND member_ipk<>?2 AND active=1",
            )?
            .query_map((conversation_id.as_slice(), me.unwrap_or([0; 32]).as_slice()), |r| {
                r.get::<_, [u8; 32]>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        super::receipts::snapshot_tx(conn, &id.to_string(), &recipients, true)?;
        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: None,
                content: content.to_string(),
                outgoing: true,
                timestamp,
                status: STATUS_PENDING,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
                system: crate::db::messages::SYSTEM_NONE,
            },
        })
    }

    /// Save an incoming (received) message. `sender` is the member who wrote
    /// it — in a group that is the inner MLS leaf credential, not the outer
    /// envelope sender. `dispatch_id` is the sender's monotonic id;
    /// `ON CONFLICT` makes redelivery a no-op — `Ok(None)` tells the caller
    /// "already have it", not an error.
    pub fn save_incoming(
        conversation_id: [u8; 16], sender: [u8; 32], dispatch_id: &[u8; 16], content: &str,
        timestamp: u64, reply_to: Option<[u8; 16]>,
    ) -> Result<Option<Self>> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let row = Self::save_incoming_tx(
            &tx,
            conversation_id,
            sender,
            dispatch_id,
            content,
            timestamp,
            reply_to,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Transaction-scoped [`Self::save_incoming`]: same insert, but against a
    /// caller-supplied connection (a `rusqlite::Transaction` derefs to
    /// `&Connection`) so an incoming media message persists its caption row and
    /// its `message_media` row in ONE transaction — a media-write failure then
    /// rolls the caption back instead of leaving a permanent caption-only
    /// orphan (the MLS ratchet is spent by receive time, so redelivery can
    /// never re-store the media). Same `Ok(None)`-on-duplicate contract.
    pub fn save_incoming_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], sender: [u8; 32],
        dispatch_id: &[u8; 16], content: &str, timestamp: u64, reply_to: Option<[u8; 16]>,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let changed = conn.execute(
            "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, reply_to, notification_seen)
             SELECT ?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, 0
             WHERE NOT EXISTS (SELECT 1 FROM message_deletions
                 WHERE conversation_id = ?2 AND sender_ipk = ?3 AND dispatch_id = ?7)
             ON CONFLICT(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (&id.to_string(), conversation_id.as_slice(), sender.as_slice(), content, timestamp, STATUS_SENT, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
        )?;

        if changed == 0 {
            return Ok(None);
        }

        super::receipts::arrived_tx(conn, &id.to_string(), systime().as_secs())?;
        Ok(Some(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: Some(sender.to_vec()),
                content: content.to_string(),
                outgoing: false,
                timestamp,
                status: STATUS_SENT,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
                system: crate::db::messages::SYSTEM_NONE,
            },
        }))
    }

    /// The outgoing row for (conversation, dispatch_id) — reloaded by the
    /// media finish path once heavy prep (compress / manifest) completes.
    pub fn get_by_dispatch(conversation_id: &[u8; 16], dispatch_id: &[u8; 16]) -> Option<Self> {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = 1",
            (conversation_id.as_slice(), dispatch_id.as_slice()),
            MessageRow::from_row,
        )
        .ok()
        .map(|inner| Self { inner })
    }

    /// Fail unaccepted recipients without overwriting anyone's receipt.
    pub fn mark_failed(id: &Ulid) {
        if let Err(e) = super::receipts::fail_pending(&id.to_string()) {
            log::warn!("MESSAGE: could not persist failure: {e}");
        }
    }

    /// Apply an edit — our own (optimistic) or an inbound peer `Edit`: replace
    /// the target's text and flag it edited. `own` is the authorship guard:
    /// only the author may edit a message, so a local edit passes `true`
    /// (touches our `outgoing = 1` rows) and an inbound peer edit passes
    /// `false` (touches only rows we received, `outgoing = 0`). Without it a
    /// peer could rewrite a message WE authored — it knows our dispatch_ids
    /// from the wire. In a group the guard tightens further via `author`: a
    /// member may only edit rows they themselves sent. No-op on an
    /// already-deleted target. Returns the updated row (for the UI event), or
    /// `None` if unauthorized/absent.
    pub fn apply_edit(
        conversation_id: &[u8; 16], dispatch_id: &[u8], content: &str, own: bool,
        author: Option<&[u8; 32]>,
    ) -> Option<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let n = conn
            .execute(
                "UPDATE messages SET content = ?1, edited = 1 \
                 WHERE conversation_id = ?2 AND dispatch_id = ?3 AND outgoing = ?4 AND deleted = 0 \
                   AND (?5 IS NULL OR sender_ipk = ?5)",
                (
                    content,
                    conversation_id.as_slice(),
                    dispatch_id,
                    own,
                    author.map(|a| a.as_slice()),
                ),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Tombstone a message (delete-for-everyone): clear its text, flag deleted,
    /// and drop its media with it — a picture that stayed in the row would
    /// make "deleted" a caption-only courtesy. Same authorship guard as
    /// [`Self::apply_edit`] — `own = true` for our own delete, `false` for an
    /// inbound peer delete, plus the per-member `author` check in a group — so
    /// nobody can tombstone another member's messages. Returns the updated row.
    pub fn apply_delete(
        conversation_id: &[u8; 16], dispatch_id: &[u8], own: bool, author: Option<&[u8; 32]>,
    ) -> Option<MessageRow> {
        if !own && let Some(author) = author {
            return Self::receive_delete(conversation_id, &dispatch_id.try_into().ok()?, author)
                .ok()
                .flatten();
        }
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction().ok()?;
        let n = tx
            .execute(
                "UPDATE messages SET content = '', deleted = 1, edited = 0 \
                 WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = ?3 \
                   AND (?4 IS NULL OR sender_ipk = ?4)",
                (conversation_id.as_slice(), dispatch_id, own, author.map(|a| a.as_slice())),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        let orphan = crate::data::media::drop_row_tx(&tx, conversation_id, dispatch_id).ok()?;
        tx.commit().ok()?;
        crate::data::media::unlink_orphaned(&conn, orphan.as_slice());
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Persist an authenticated author's deletion even when its post has not
    /// arrived yet. Recording the marker, tombstoning the row and removing its
    /// media are one transaction; file cleanup runs only after commit. No
    /// phantom message/notification is created for an unknown target.
    pub(crate) fn receive_delete(
        conversation_id: &[u8; 16], dispatch_id: &[u8; 16], author: &[u8; 32],
    ) -> Result<Option<MessageRow>> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let (row, orphan) = Self::receive_delete_tx(&tx, conversation_id, dispatch_id, author)?;
        tx.commit()?;
        crate::data::media::unlink_orphaned(&conn, orphan.as_slice());
        Ok(row)
    }

    fn receive_delete_tx(
        tx: &rusqlite::Transaction<'_>, conversation_id: &[u8; 16], dispatch_id: &[u8; 16],
        author: &[u8; 32],
    ) -> Result<(Option<MessageRow>, Option<[u8; 32]>)> {
        tx.execute(
            "INSERT OR IGNORE INTO message_deletions (conversation_id, sender_ipk, dispatch_id)
             VALUES (?1, ?2, ?3)",
            (conversation_id.as_slice(), author.as_slice(), dispatch_id.as_slice()),
        )?;
        let n = tx.execute(
            "UPDATE messages SET content = '', deleted = 1, edited = 0
             WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = 0
               AND sender_ipk = ?3 AND deleted = 0",
            (conversation_id.as_slice(), dispatch_id.as_slice(), author.as_slice()),
        )?;
        if n == 0 {
            return Ok((None, None));
        }
        let orphan = crate::data::media::drop_row_tx(tx, conversation_id, dispatch_id)?;
        let row = tx.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id.as_slice()),
            MessageRow::from_row,
        )?;
        Ok((Some(row), orphan))
    }

    /// Hard-delete a single message locally (delete-for-me; no wire signal),
    /// media and all. Returns the row it removed (for the UI event), or `None`
    /// if absent.
    pub fn hard_delete(conversation_id: &[u8; 16], dispatch_id: &[u8]) -> Option<MessageRow> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction().ok()?;
        let row = tx
            .query_row(
                "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
                (conversation_id.as_slice(), dispatch_id),
                MessageRow::from_row,
            )
            .ok()?;
        tx.execute(
            "DELETE FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
        )
        .ok()?;
        let orphan = crate::data::media::drop_row_tx(&tx, conversation_id, dispatch_id).ok()?;
        tx.commit().ok()?;
        crate::data::media::unlink_orphaned(&conn, orphan.as_slice());
        Some(row)
    }

    /// Count only this message's original recipients with read evidence.
    pub fn seen_by_count(conversation_id: &[u8; 16], dispatch_id: &[u8; 16]) -> u32 {
        super::receipts::seen_count(conversation_id, dispatch_id)
    }

    /// Record every notice for one signed transition atomically. The marker
    /// survives clearing history, so recovery cannot recreate cleared notices.
    pub(crate) fn record_group_change(
        conversation: [u8; 16], change: [u8; 32], actor: [u8; 32], rows: &[(u8, String)], ts: u64,
    ) -> Result<Option<Vec<Self>>> {
        use sha2::Digest;
        use sha2::Sha256;
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        if tx.execute(
            "INSERT OR IGNORE INTO group_events(conversation_id,change_id) VALUES(?1,?2)",
            rusqlite::params![conversation, change],
        )? == 0
        {
            return Ok(None);
        }
        let ours = super::identity::Identity::get().is_some_and(|i| i.ipk() == actor);
        let mut saved = Vec::new();
        for (code, target) in rows {
            let hash =
                Sha256::digest(postcard::to_allocvec(&(conversation, change, code, target))?);
            let did: [u8; 16] = hash[..16].try_into()?;
            if let Some(row) = Self::save_system_tx(
                &tx,
                conversation,
                actor,
                &did,
                *code,
                target,
                ts,
                ours,
                Some(&change),
            )? {
                saved.push(row);
            }
        }
        tx.commit()?;
        Ok(Some(saved))
    }

    pub(crate) fn reconcile_group_events(
        conversation: &[u8; 16], accepted: &[[u8; 32]],
    ) -> Result<()> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let stored = {
            let mut query = tx.prepare("SELECT DISTINCT group_change FROM messages WHERE conversation_id=?1 AND group_change IS NOT NULL")?;
            query
                .query_map([conversation], |r| r.get::<_, [u8; 32]>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for change in stored {
            if !accepted.contains(&change) {
                tx.execute(
                    "DELETE FROM messages WHERE conversation_id=?1 AND group_change=?2",
                    rusqlite::params![conversation, change],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Narrate a membership or title change inline with the conversation.
    /// `actor` is who did it; `target` names the affected member (hex IPK) or
    /// the new title. Deduped on `(conversation, dispatch_id)` like any other
    /// message, so a redelivered announcement lands once.
    pub fn save_system(
        conversation_id: [u8; 16], actor: [u8; 32], dispatch_id: &[u8; 16], system: u8,
        target: &str, timestamp: u64, outgoing: bool,
    ) -> Result<Option<Self>> {
        let conn = MESSAGES_DB.lock();
        Self::save_system_tx(
            &conn,
            conversation_id,
            actor,
            dispatch_id,
            system,
            target,
            timestamp,
            outgoing,
            None,
        )
    }

    fn save_system_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], actor: [u8; 32],
        dispatch_id: &[u8; 16], system: u8, target: &str, timestamp: u64, outgoing: bool,
        group_change: Option<&[u8; 32]>,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let changed = conn.execute(
            "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, system, group_change) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (
                &id.to_string(),
                conversation_id.as_slice(),
                actor.as_slice(),
                target,
                outgoing,
                timestamp,
                STATUS_SENT,
                dispatch_id.as_slice(),
                system,
                group_change.map(|c| c.as_slice()),
            ),
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: Some(actor.to_vec()),
                content: target.to_string(),
                outgoing,
                timestamp,
                status: STATUS_SENT,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: None,
                system,
            },
        }))
    }

    /// Every message, oldest first — the backup dump (IDENTITY_RECOVERY.md §4).
    pub fn dump_all() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare("SELECT * FROM messages ORDER BY id ASC") else {
            return Vec::new();
        };
        stmt.query_map([], MessageRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Restore dumped rows in one transaction. `INSERT OR IGNORE` — the ULID
    /// PK plus the `(conversation_id, dispatch_id)` partial index make
    /// re-imports idempotent. Returns rows actually inserted.
    pub fn import_rows(rows: &[MessageRow]) -> Result<usize> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let mut n = 0usize;
        for r in rows {
            n += tx.execute(
                "INSERT OR IGNORE INTO messages \
                 (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to, system) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                (
                    &r.id,
                    r.conversation_id.as_slice(),
                    &r.sender_ipk,
                    &r.content,
                    r.outgoing,
                    r.timestamp,
                    r.status,
                    &r.dispatch_id,
                    r.edited,
                    r.deleted,
                    &r.reply_to,
                    r.system,
                ),
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Additive twin of [`Self::import_rows`], for symmetry with the other
    /// tables' `merge_rows`. Messages need no separate SQL: `import_rows` is
    /// already `INSERT OR IGNORE`, so a row we hold always outranks the blob's.
    pub fn merge_rows(rows: &[MessageRow]) -> Result<usize> {
        Self::import_rows(rows)
    }

    /// Delete every message in a conversation (forget-contact / leave cascade).
    pub fn delete_in(conversation_id: &[u8; 16]) {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            [conversation_id.as_slice()],
        )
        .ok();
    }

    /// Fail every not-yet-read outgoing message in a conversation (PAIRING.md):
    /// a declined pair means our PENDING-era sends were encrypted to a group the
    /// peer never joined, so they can never arrive. Skips already-read/delivered
    /// (status > sent) defensively. Rides the reactive doorbell.
    pub fn mark_all_failed_in(conversation_id: &[u8; 16]) {
        if let Err(e) = super::receipts::reject_conversation(conversation_id) {
            log::warn!("MESSAGE: could not persist declined-message outcomes: {e}");
        }
    }

    /// Count of messages in a conversation (cheap diagnostics read).
    pub fn count_in(conversation_id: &[u8; 16]) -> u32 {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND deleted = 0",
            [conversation_id.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u32)
        .unwrap_or(0)
    }

    /// Status of the newest message in a conversation, or `None` if none.
    pub fn last_status_in(conversation_id: &[u8; 16]) -> Option<u8> {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT status FROM messages WHERE conversation_id = ?1 AND deleted = 0 ORDER BY id DESC LIMIT 1",
            [conversation_id.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .ok()
        .map(|s| s as u8)
    }

    /// Get messages for a conversation, paginated.
    /// Returns messages in ascending order (oldest first).
    /// `before_id` if non-empty, fetches messages before that ULID.
    pub fn get_messages(
        conversation_id: &[u8; 16], limit: u32, before_id: &str,
    ) -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();

        if !before_id.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM messages WHERE conversation_id = ?1 AND deleted = 0 AND id < ?2 ORDER BY id DESC LIMIT ?3",
                )
                .expect("failed to prepare");
            let mut rows: Vec<MessageRow> = stmt
                .query_map((conversation_id.as_slice(), before_id, limit), MessageRow::from_row)
                .expect("failed to query")
                .filter_map(|r| r.ok())
                .collect();
            rows.reverse();
            rows
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM messages WHERE conversation_id = ?1 AND deleted = 0 ORDER BY id DESC LIMIT ?2",
                )
                .expect("failed to prepare");
            let mut rows: Vec<MessageRow> = stmt
                .query_map((conversation_id.as_slice(), limit), MessageRow::from_row)
                .expect("failed to query")
                .filter_map(|r| r.ok())
                .collect();
            rows.reverse();
            rows
        }
    }

    /// Messages in `conversation` whose text contains `query`, newest first,
    /// each with how many messages in the chat are newer than it — what a
    /// screen holding the newest N needs to know to widen its window to the
    /// hit. Plain substring match; case-insensitive for ASCII, which is what
    /// SQLite's LIKE gives without ICU.
    ///
    /// ponytail: LIKE over `content` — an FTS table if a chat ever holds
    /// enough to make this noticeable.
    pub fn search(conversation_id: &[u8; 16], query: &str, limit: u32) -> Vec<([u8; 16], u32)> {
        let needle = query.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let pattern =
            format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let conn = MESSAGES_DB.lock();
        let mut stmt = match conn.prepare(
            "SELECT m.dispatch_id, \
                    (SELECT COUNT(*) FROM messages n \
                      WHERE n.conversation_id = m.conversation_id AND n.deleted = 0 AND n.id > m.id) \
             FROM messages m \
             WHERE m.conversation_id = ?1 AND m.deleted = 0 AND m.system = 0 \
               AND m.dispatch_id IS NOT NULL AND m.content LIKE ?2 ESCAPE '\\' \
             ORDER BY m.id DESC LIMIT ?3",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map((conversation_id.as_slice(), pattern, limit), |r| {
            let did: Vec<u8> = r.get(0)?;
            Ok((did, r.get::<_, u32>(1)?))
        })
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .filter_map(|(did, newer)| Some((did.try_into().ok()?, newer)))
                .collect()
        })
        .unwrap_or_default()
    }

    /// Window depth of the first message on/after a local day's start (UTC seconds).
    /// If the date is after all history, use the latest timestamp. Timestamp and
    /// insertion order may differ after an offline drain, so count by id only
    /// after choosing the target by timestamp.
    pub fn position_at_time(
        conversation_id: &[u8; 16], timestamp: u64,
    ) -> Result<Option<(String, Option<Vec<u8>>, u32)>> {
        position_at_time(&MESSAGES_DB.lock(), conversation_id, timestamp)
    }

    /// Outgoing rows still pending (status = 0) — the durable-first-send
    /// retry set. Oldest-first by ULID so a reconnect re-sends in send order.
    pub fn pending_outgoing() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare(
                "SELECT * FROM messages WHERE outgoing = 1 AND status = 0 AND deleted = 0 \
                 ORDER BY id ASC",
            )
            .expect("failed to prepare");
        stmt.query_map([], MessageRow::from_row)
            .expect("failed to query")
            .filter_map(|r| r.ok())
            .collect()
    }

    /// The latest message in each conversation — the home list's preview line.
    pub fn get_conversations() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.* FROM messages m
                 INNER JOIN (
                     SELECT conversation_id, MAX(id) AS max_id FROM messages WHERE deleted = 0 GROUP BY conversation_id
                 ) latest ON m.id = latest.max_id
                 ORDER BY m.id DESC",
            )
            .expect("failed to prepare");
        stmt.query_map([], MessageRow::from_row)
            .expect("failed to query")
            .filter_map(|r| r.ok())
            .collect()
    }

    /// Newest incoming (dispatch-bearing) message's id in a conversation — the
    /// watermark target when marking a whole conversation read.
    pub fn newest_incoming_dispatch(conversation_id: &[u8; 16]) -> Option<[u8; 16]> {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT dispatch_id FROM messages
             WHERE conversation_id = ?1 AND outgoing = 0 AND dispatch_id IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [conversation_id.as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// Unread incoming count per conversation: incoming, non-deleted,
    /// dispatch-bearing messages newer than that conversation's read
    /// watermark. Only conversations with unread > 0.
    pub fn unread_counts() -> Vec<([u8; 16], u32)> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.conversation_id, COUNT(*) FROM messages m
                 LEFT JOIN incoming_receipts r ON r.message_id = m.id
                 LEFT JOIN read_state legacy ON legacy.conversation_id = m.conversation_id
                 WHERE m.outgoing = 0 AND m.deleted = 0 AND m.system = 0 AND m.dispatch_id IS NOT NULL
                   AND CASE WHEN r.message_id IS NOT NULL THEN r.is_read=0
                       ELSE legacy.upto_dispatch_id IS NULL OR m.dispatch_id>legacy.upto_dispatch_id END
                 GROUP BY m.conversation_id",
            )
            .expect("failed to prepare");
        stmt.query_map([], |row| {
            let conv: Vec<u8> = row.get(0)?;
            let count: u32 = row.get(1)?;
            Ok((conv.try_into().unwrap_or([0u8; 16]), count))
        })
        .expect("failed to query")
        .filter_map(|r| r.ok())
        .collect()
    }
}

fn position_at_time(
    conn: &rusqlite::Connection, conversation: &[u8; 16], timestamp: u64,
) -> Result<Option<(String, Option<Vec<u8>>, u32)>> {
    use rusqlite::OptionalExtension;
    Ok(conn.query_row(
        "SELECT m.id, m.dispatch_id, (SELECT COUNT(*) FROM messages n WHERE n.conversation_id = m.conversation_id AND n.deleted = 0 AND n.id > m.id) \
         FROM messages m WHERE m.conversation_id = ?1 AND m.deleted = 0 \
         ORDER BY CASE WHEN m.timestamp >= ?2 THEN 0 ELSE 1 END, \
                  CASE WHEN m.timestamp >= ?2 THEN m.timestamp END ASC, \
                  m.timestamp DESC, m.id ASC LIMIT 1",
        (conversation.as_slice(), timestamp), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_deletion_is_author_scoped_durable_and_transactional() {
        let mut conn = crate::db::messages::open_in_memory();
        let conv = [0xD3; 16];
        let author = [0xD4; 32];
        let other = [0xD5; 32];
        let target = [0xD6; 16];
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::receive_delete_tx(&tx, &conv, &target, &author).unwrap().0.is_none());
            tx.commit().unwrap();
        }
        // Reopen the persisted database, not an in-memory dedup set.
        let path =
            std::env::temp_dir().join(format!("promtuz-deletions-{}.sqlite", uuid::Uuid::now_v7()));
        conn.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
        drop(conn);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert!(
            Message::save_incoming_tx(&conn, conv, author, &target, "late post", 1, None)
                .unwrap()
                .is_none()
        );
        assert!(
            Message::save_incoming_tx(&conn, conv, other, &target, "another member", 1, None)
                .unwrap()
                .is_some()
        );
        {
            let tx = conn.transaction().unwrap();
            // A repeated deletion cannot affect another author's same ID.
            assert!(Message::receive_delete_tx(&tx, &conv, &target, &author).unwrap().0.is_none());
            tx.commit().unwrap();
        }
        let gone: bool = conn
            .query_row(
                "SELECT deleted FROM messages WHERE conversation_id=?1 AND dispatch_id=?2",
                (conv.as_slice(), target.as_slice()),
                |r| r.get(0),
            )
            .unwrap();
        assert!(!gone);
        // A failed transaction must neither consume the future post nor leave
        // a half-applied tombstone. A trigger simulates a storage-write error.
        let failed = [0xD7; 16];
        Message::save_incoming_tx(&conn, conv, author, &failed, "keep", 1, None).unwrap();
        conn.execute_batch("CREATE TRIGGER reject_delete BEFORE UPDATE ON messages BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::receive_delete_tx(&tx, &conv, &failed, &author).is_err());
        }
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_deletions WHERE dispatch_id=?1",
                [failed.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        conn.execute_batch("DROP TRIGGER reject_delete").unwrap();
        crate::data::conversation::Conversation::clear_history_tx(&conn, &conv).unwrap();
        assert!(
            Message::save_incoming_tx(&conn, conv, author, &target, "after clear", 1, None)
                .unwrap()
                .is_none()
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn calendar_depth_handles_late_arrivals_gaps_and_conversation_boundaries() {
        let conn = crate::db::messages::open_in_memory();
        let conv = [41; 16];
        let other = [42; 16];
        // Arrival order deliberately differs from message dates; calendar
        // targeting uses dates, but pagination still uses insertion IDs.
        for (id, timestamp, conversation) in [
            ("01", 100, conv),
            ("02", 300, conv),
            ("03", 200, conv),
            ("04", 201, other),
            ("05", 300, conv),
        ] {
            conn.execute(
                "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status) VALUES (?1, ?2, '', 1, ?3, 1)",
                (id, conversation.as_slice(), timestamp),
            ).unwrap();
        }
        assert_eq!(position_at_time(&conn, &conv, 0).unwrap().map(|p| p.2), Some(3));
        assert_eq!(position_at_time(&conn, &conv, 200).unwrap().map(|p| p.2), Some(1));
        assert_eq!(position_at_time(&conn, &conv, 201).unwrap().map(|p| p.2), Some(2));
        assert_eq!(position_at_time(&conn, &conv, 300).unwrap().map(|p| p.2), Some(2));
        assert_eq!(position_at_time(&conn, &conv, 400).unwrap().map(|p| p.2), Some(2));
        assert_eq!(position_at_time(&conn, &[43; 16], 0).unwrap(), None);
    }

    #[test]
    fn dispatch_id_is_monotonic() {
        let a = next_dispatch_id();
        let b = next_dispatch_id();
        assert!(b > a, "ids must strictly increase");
    }

    #[test]
    fn notification_acknowledgement_does_not_consume_a_racing_arrival() {
        let db = crate::db::messages::open_in_memory();
        let conversation = [8; 16];
        let sender = [7; 32];
        let insert = |did: [u8; 16], time| {
            Message::save_incoming_tx(&db, conversation, sender, &did, "message", time, None)
                .unwrap();
        };
        insert([1; 16], 100);
        let first = pending_notification_ids(&db, &conversation).unwrap();
        assert_eq!(first.len(), 1);
        insert([2; 16], 100);
        mark_notified(&db, &first).unwrap();
        let next = pending_notification_ids(&db, &conversation).unwrap();
        assert_eq!(next.len(), 1);
        assert_ne!(next, first);
        mark_notified(&db, &next).unwrap();
        // An older sender clock and an already-delivered dispatch are distinct cases.
        insert([3; 16], 90);
        insert([2; 16], 100);
        assert_eq!(pending_notification_ids(&db, &conversation).unwrap().len(), 1);
        crate::data::receipts::read_tx(&db, &conversation, &[3; 16], 110).unwrap();
        assert!(pending_notification_ids(&db, &conversation).unwrap().is_empty());
    }

    /// `save_incoming` runs through the process-global `MESSAGES_DB`
    /// Lazy, which is fragile to test directly (path resolves once from
    /// `PROMTUZ_DATA_DIR`). Exercise the same SQL against an in-memory
    /// connection instead: the `(conversation_id, dispatch_id)` partial unique
    /// index + `ON CONFLICT DO NOTHING` is exactly what `save_incoming`
    /// relies on for idempotence.
    #[test]
    fn save_incoming_dedups_on_dispatch_id() {
        let conn = crate::db::messages::open_in_memory();
        let conv = [7u8; 16];
        let did = [1u8; 16];
        let sql = "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status, dispatch_id) \
                   VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6) \
                   ON CONFLICT(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING";

        let first = conn
            .execute(
                sql,
                (
                    Ulid::new().to_string(),
                    conv.as_slice(),
                    "hi",
                    100u64,
                    STATUS_SENT,
                    did.as_slice(),
                ),
            )
            .unwrap();
        let dup = conn
            .execute(
                sql,
                (
                    Ulid::new().to_string(),
                    conv.as_slice(),
                    "hi",
                    100u64,
                    STATUS_SENT,
                    did.as_slice(),
                ),
            )
            .unwrap();

        assert_eq!(first, 1, "first insert must land");
        assert_eq!(dup, 0, "same (conversation, dispatch_id) must not double-insert");
    }

    /// The receipt high-water-mark: `dispatch_id <= upto` must order by the
    /// 16-byte BE id (so one receipt covers the backlog), and `status < ?` must
    /// never downgrade (a later Delivered can't undo a Read). Mirrors
    /// `mark_receipt_upto`'s SQL against an in-memory DB (the method uses the
    /// process-global connection).
    /// Newest first, each hit knowing how far down the chat it sits, and the
    /// LIKE wildcards a user might type treated as text.
    #[test]
    fn search_finds_text_newest_first_with_its_depth() {
        let dir = std::env::temp_dir().join("promtuz-search-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        // The data dir outlives the run; a fresh chat each time keeps the
        // counts honest.
        let conv: [u8; 16] = Ulid::new().to_bytes();
        // Rows are ordered by ULID, which only orders across milliseconds.
        let tick = || std::thread::sleep(std::time::Duration::from_millis(2));
        let first = Message::save_outgoing(conv, "hello world", None).unwrap();
        tick();
        Message::save_outgoing(conv, "HELLO again", None).unwrap();
        tick();
        Message::save_outgoing(conv, "bye", None).unwrap();

        let hits = Message::search(&conv, "hello", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1, 1, "the newer hit has one message after it");
        assert_eq!(hits[1].1, 2, "the older has two");
        let first_did: [u8; 16] = first.inner.dispatch_id.unwrap().try_into().unwrap();
        assert_eq!(hits[1].0, first_did);

        assert!(Message::search(&conv, "%", 10).is_empty(), "a wildcard is just a character");
        assert!(Message::search(&conv, "  ", 10).is_empty(), "blank finds nothing");
    }

    /// A row written before the `dispatch_id` column existed has NULL there.
    /// `MessageRow::from_row` must decode NULL → `None`, not error — otherwise
    /// the `filter_map(Result::ok)` readers silently drop every legacy row.
    #[test]
    fn legacy_null_dispatch_id_row_reads_back() {
        let conn = crate::db::messages::open_in_memory();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status) \
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            (Ulid::new().to_string(), [9u8; 16].as_slice(), "legacy", 42u64, STATUS_SENT),
        )
        .unwrap();

        let row = conn.query_row("SELECT * FROM messages", [], MessageRow::from_row).unwrap();
        assert_eq!(row.dispatch_id, None, "NULL dispatch_id must decode to None");
        assert_eq!(row.sender_ipk, None, "no sender stored → resolves to the local user");
        assert_eq!(row.sender(&[5u8; 32]), [5u8; 32]);
    }
}

/// Both read-watermark tables, for the backup snapshot.
pub fn dump_read_state()
-> (Vec<crate::data::backup::ReadRow>, Vec<crate::data::backup::MemberReadRow>) {
    use crate::data::backup::MemberReadRow;
    use crate::data::backup::ReadRow;

    let conn = MESSAGES_DB.lock();
    let mine = conn
        .prepare("SELECT conversation_id, upto_dispatch_id FROM read_state")
        .and_then(|mut s| {
            s.query_map([], |r| {
                let conv: Vec<u8> = r.get(0)?;
                Ok(ReadRow {
                    conversation_id:  conv.try_into().unwrap_or([0u8; 16]),
                    upto_dispatch_id: r.get(1)?,
                })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    let theirs = conn
        .prepare("SELECT conversation_id, member_ipk, upto_dispatch_id FROM member_read_state")
        .and_then(|mut s| {
            s.query_map([], |r| {
                let conv: Vec<u8> = r.get(0)?;
                let who: Vec<u8> = r.get(1)?;
                Ok(MemberReadRow {
                    conversation_id:  conv.try_into().unwrap_or([0u8; 16]),
                    member_ipk:       who.try_into().unwrap_or([0u8; 32]),
                    upto_dispatch_id: r.get(2)?,
                })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    (mine, theirs)
}

/// Restore read watermarks. `INSERT OR IGNORE`: a live watermark is always
/// further along than a snapshot's, so it must win.
pub fn import_read_state(
    mine: &[crate::data::backup::ReadRow], theirs: &[crate::data::backup::MemberReadRow],
) -> Result<()> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    for r in mine {
        tx.execute(
            "INSERT OR IGNORE INTO read_state (conversation_id, upto_dispatch_id) VALUES (?1, ?2)",
            (r.conversation_id.as_slice(), &r.upto_dispatch_id),
        )?;
    }
    for r in theirs {
        tx.execute(
            "INSERT OR IGNORE INTO member_read_state \
             (conversation_id, member_ipk, upto_dispatch_id) VALUES (?1, ?2, ?3)",
            (r.conversation_id.as_slice(), r.member_ipk.as_slice(), &r.upto_dispatch_id),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The newest `limit` incoming, undeleted messages in a conversation, returned
/// oldest-first — the lines a notification shows.
///
/// The rule lives here rather than in each platform's notification code: it is
/// a query about messages, and Android and iOS would otherwise each write their
/// own filter-and-sort over a full page of rows.
impl Message {
    /// Snapshot only unseen, unread arrivals. Message ids avoid sender clock
    /// skew and multiple messages in the same timestamp second.
    pub fn pending_notification_ids(conversation_id: &[u8; 16]) -> Result<Vec<String>> {
        pending_notification_ids(&MESSAGES_DB.lock(), conversation_id)
    }

    pub fn mark_notified(ids: &[String]) -> Result<()> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        mark_notified(&tx, ids)?;
        tx.commit()?;
        Ok(())
    }

    pub fn recent_incoming(conversation_id: &[u8; 16], limit: u32) -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT * FROM messages \
             WHERE conversation_id = ?1 AND outgoing = 0 AND deleted = 0 \
             ORDER BY id DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        let mut rows: Vec<MessageRow> = stmt
            .query_map((conversation_id.as_slice(), limit), MessageRow::from_row)
            .map(|r| r.flatten().collect())
            .unwrap_or_default();
        rows.reverse();
        rows
    }
}

fn pending_notification_ids(
    conn: &rusqlite::Connection, conversation_id: &[u8; 16],
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT m.id FROM messages m
         LEFT JOIN incoming_receipts r ON r.message_id=m.id
         LEFT JOIN read_state legacy ON legacy.conversation_id=m.conversation_id
         WHERE m.conversation_id = ?1 AND m.outgoing = 0 AND m.deleted = 0 AND m.system=0
           AND m.notification_seen = 0 AND m.dispatch_id IS NOT NULL
           AND CASE WHEN r.message_id IS NOT NULL THEN r.is_read=0
               ELSE legacy.upto_dispatch_id IS NULL OR m.dispatch_id>legacy.upto_dispatch_id END",
    )?;
    Ok(stmt
        .query_map([conversation_id.as_slice()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn mark_notified(conn: &rusqlite::Connection, ids: &[String]) -> Result<()> {
    let mut stmt = conn.prepare(
        "UPDATE messages SET notification_seen = 1 WHERE id = ?1 AND notification_seen = 0",
    )?;
    for id in ids { stmt.execute([id])?; }
    Ok(())
}
