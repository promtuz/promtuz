//! Conversations — the chat scope every message, reaction, receipt and media
//! row hangs off.
//!
//! A conversation is identified by a locally-minted 16-byte id that never
//! changes, and points at the MLS group currently backing it. The pointer is
//! deliberately *not* the identity: three paths re-mint a group id under a live
//! conversation — the send path recreating a group whose local state went
//! missing, [`heal_dead_group`] re-establishing after a restore, and an inbound
//! Welcome adopting the peer's group on a re-pair. History keyed on the group
//! id would be orphaned by each of them.
//!
//! A 1:1 chat is the degenerate case: `kind = DIRECT`, two members. One code
//! path serves 1 and N.
//!
//! [`heal_dead_group`]: crate::messaging

use anyhow::Result;
use anyhow::anyhow;
use rusqlite::Connection;
use ulid::Ulid;

use crate::data::identity::Identity;
use crate::db::messages::ConversationRow;
use crate::db::messages::MESSAGES_DB;
use crate::db::messages::MemberRow;
use crate::mls::GroupMeta;
use crate::mls::GroupState;
use crate::utils::systime;

/// Two-party chat, titled by the peer's contact name.
pub const KIND_DIRECT: u8 = 0;
/// Multi-member chat, carrying its own title and roster.
pub const KIND_GROUP: u8 = 1;

pub use crate::mls::policy::ROLE_ADMIN;
pub use crate::mls::policy::ROLE_MEMBER;
pub use crate::mls::policy::ROLE_OWNER;

/// Time-sortable, so an unordered conversation list still reads oldest-first.
fn mint_conversation_id() -> [u8; 16] {
    Ulid::new().to_bytes()
}

const MAX_TITLE: usize = 64;

pub struct Conversation;

impl Conversation {
    pub fn get(id: &[u8; 16]) -> Option<ConversationRow> {
        let conn = MESSAGES_DB.lock();
        Self::get_tx(&conn, id)
    }

    pub fn get_tx(conn: &Connection, id: &[u8; 16]) -> Option<ConversationRow> {
        conn.query_row(
            "SELECT * FROM conversations WHERE id = ?1",
            [id.as_slice()],
            ConversationRow::from_row,
        )
        .ok()
    }

    /// The direct conversation with `peer`, created if this is the first time
    /// we've needed one. Every 1:1 entry point funnels through here, so a
    /// conversation exists by the time anything wants to write against it.
    pub fn for_peer(peer: &[u8; 32]) -> Result<[u8; 16]> {
        // Resolve identity before taking the messages lock — `Identity` reads
        // its own database, and nesting the two locks in both orders would
        // eventually deadlock.
        let me = Identity::get().map(|i| i.ipk());
        let conn = MESSAGES_DB.lock();
        Self::for_peer_tx(&conn, peer, me)
    }

    /// Transaction-scoped [`Self::for_peer`]. `me` is the local IPK when
    /// known; passing `None` just defers our own roster row to a later call.
    pub fn for_peer_tx(
        conn: &Connection, peer: &[u8; 32], me: Option<[u8; 32]>,
    ) -> Result<[u8; 16]> {
        if let Some(id) = Self::find_direct(conn, peer)? {
            // Backfilled rows carry only the peer; add ourselves once we can.
            if let Some(me) = me {
                Self::put_member(conn, &id, &me, ROLE_MEMBER)?;
            }
            return Ok(id);
        }

        let id = mint_conversation_id();
        let now = systime().as_secs();
        conn.execute(
            "INSERT INTO conversations (id, kind, title, mls_group_id, created_at, created_by) \
             VALUES (?1, ?2, '', NULL, ?3, NULL)",
            (id.as_slice(), KIND_DIRECT, now),
        )?;
        Self::put_member(conn, &id, peer, ROLE_MEMBER)?;
        if let Some(me) = me {
            Self::put_member(conn, &id, &me, ROLE_MEMBER)?;
        }
        Ok(id)
    }

    fn find_direct(conn: &Connection, peer: &[u8; 32]) -> Result<Option<[u8; 16]>> {
        let found = conn
            .query_row(
                "SELECT c.id FROM conversations c \
                 JOIN conversation_members m ON m.conversation_id = c.id \
                 WHERE c.kind = ?1 AND m.member_ipk = ?2 LIMIT 1",
                (KIND_DIRECT, peer.as_slice()),
                |r| r.get::<_, Vec<u8>>(0),
            )
            .ok()
            .and_then(|v| v.try_into().ok());
        Ok(found)
    }

    /// Create a group conversation with us as its owner and `members` as the
    /// initial roster. The MLS group is bound separately once it exists.
    pub fn create_group(title: &str, members: &[[u8; 32]]) -> Result<[u8; 16]> {
        let me = Identity::get().map(|i| i.ipk());
        let conn = MESSAGES_DB.lock();
        let id = mint_conversation_id();
        let now = systime().as_secs();
        conn.execute(
            "INSERT INTO conversations (id, kind, title, mls_group_id, created_at, created_by) \
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            (id.as_slice(), KIND_GROUP, title, now, me.as_ref().map(|m| m.as_slice())),
        )?;
        if let Some(me) = me {
            Self::put_member(&conn, &id, &me, ROLE_OWNER)?;
        }
        for m in members {
            Self::put_member(&conn, &id, m, ROLE_MEMBER)?;
        }
        Ok(id)
    }

    /// Create a group conversation we were Welcomed into, rather than founded.
    ///
    /// The roster comes from the MLS group itself — it is the authority on who
    /// is in it, and it already includes us. `creator` founded it and starts as
    /// its owner, until [`Self::sync_group`] reads the roles off the group.
    pub fn join_group(creator: &[u8; 32], members: &[[u8; 32]]) -> Result<[u8; 16]> {
        let conn = MESSAGES_DB.lock();
        Self::join_group_tx(&conn, creator, members)
    }

    pub fn join_group_tx(
        conn: &Connection, creator: &[u8; 32], members: &[[u8; 32]],
    ) -> Result<[u8; 16]> {
        let id = mint_conversation_id();
        let now = systime().as_secs();
        conn.execute(
            "INSERT INTO conversations (id, kind, title, mls_group_id, created_at, created_by) \
             VALUES (?1, ?2, '', NULL, ?3, ?4)",
            (id.as_slice(), KIND_GROUP, now, creator.as_slice()),
        )?;
        Self::put_member(&conn, &id, creator, ROLE_OWNER)?;
        for m in members.iter().filter(|m| *m != creator) {
            Self::put_member(&conn, &id, m, ROLE_MEMBER)?;
        }
        Ok(id)
    }

    /// The conversation currently backed by this MLS group, if any.
    pub fn for_group(group_id: &[u8; 32]) -> Option<[u8; 16]> {
        let conn = MESSAGES_DB.lock();
        Self::for_group_tx(&conn, group_id)
    }

    pub fn for_group_tx(conn: &Connection, group_id: &[u8; 32]) -> Option<[u8; 16]> {
        conn.query_row(
            "SELECT id FROM conversations WHERE mls_group_id = ?1",
            [group_id.as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// Resolve a shared activity address, retaining the active-member gate.
    pub(crate) fn for_activity(group_id: &[u8; 32], sender: &[u8; 32]) -> Option<[u8; 16]> {
        Self::for_activity_tx(&MESSAGES_DB.lock(), group_id, sender)
    }

    fn for_activity_tx(
        conn: &Connection, group_id: &[u8; 32], sender: &[u8; 32],
    ) -> Option<[u8; 16]> {
        conn.query_row(
            "SELECT c.id FROM conversations c \
             JOIN conversation_members m ON m.conversation_id = c.id \
             WHERE c.mls_group_id = ?1 AND m.member_ipk = ?2 AND m.active = 1",
            (group_id.as_slice(), sender.as_slice()),
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// Point `id` at `group_id`, releasing whatever conversation held that
    /// group before. The unique index means a group backs at most one
    /// conversation, so a re-pair that adopts the peer's group has to evict
    /// the stale pointer rather than fail — the evicted conversation keeps its
    /// history and simply has no live group until it re-establishes.
    pub fn bind_group(id: &[u8; 16], group_id: &[u8; 32]) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        Self::bind_group_tx(&conn, id, group_id)
    }

    pub fn bind_group_tx(conn: &Connection, id: &[u8; 16], group_id: &[u8; 32]) -> Result<()> {
        conn.execute(
            "UPDATE conversations SET mls_group_id = NULL WHERE mls_group_id = ?1 AND id <> ?2",
            (group_id.as_slice(), id.as_slice()),
        )?;
        conn.execute(
            "UPDATE conversations SET mls_group_id = ?1 WHERE id = ?2",
            (group_id.as_slice(), id.as_slice()),
        )?;
        Ok(())
    }

    pub fn unbind_group(id: &[u8; 16]) -> Result<()> {
        MESSAGES_DB.lock().execute(
            "UPDATE conversations SET mls_group_id = NULL WHERE id = ?1",
            [id.as_slice()],
        )?;
        Ok(())
    }

    /// The MLS group backing this conversation, if one has been created.
    pub fn group_of(id: &[u8; 16]) -> Option<[u8; 32]> {
        Self::get(id).and_then(|c| c.mls_group_id).and_then(|v| v.try_into().ok())
    }

    /// The other party of a direct conversation — the IPK the send path still
    /// needs to address a dispatch. `None` for a group, which has no single
    /// counterpart.
    pub fn peer_of(id: &[u8; 16]) -> Option<[u8; 32]> {
        let me = Identity::get().map(|i| i.ipk());
        let conn = MESSAGES_DB.lock();
        Self::peer_of_tx(&conn, id, me)
    }

    pub fn peer_of_tx(conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>) -> Option<[u8; 32]> {
        let me = me.unwrap_or([0u8; 32]);
        conn.query_row(
            "SELECT member_ipk FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk <> ?2 LIMIT 1",
            (id.as_slice(), me.as_slice()),
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// An existing encrypted chat usable for peer transport signaling. Never
    /// create a direct chat as a side effect of downloading group media.
    /// Unpaired peers may use only a group we are both still members of.
    pub(crate) fn for_peer_transport(peer: &[u8; 32], paired: bool) -> Option<[u8; 16]> {
        let me = Identity::get()?.ipk();
        Self::for_peer_transport_tx(&MESSAGES_DB.lock(), &me, peer, paired)
    }

    fn for_peer_transport_tx(
        conn: &Connection, me: &[u8; 32], peer: &[u8; 32], paired: bool,
    ) -> Option<[u8; 16]> {
        if me == peer {
            return None;
        }
        // Migrated direct chats may have only the peer's roster row. Pairing
        // authorizes those; group transport always requires both active rows.
        conn.query_row(
            "SELECT c.id FROM conversations c
             LEFT JOIN conversation_members mine
               ON mine.conversation_id = c.id AND mine.member_ipk = ?1
             JOIN conversation_members theirs ON theirs.conversation_id = c.id
             WHERE theirs.member_ipk = ?2 AND theirs.active = 1
               AND c.mls_group_id IS NOT NULL
               AND ((c.kind = ?3 AND mine.active = 1) OR (c.kind = ?4 AND ?5))
             ORDER BY c.kind, c.id LIMIT 1",
            rusqlite::params![me.as_slice(), peer.as_slice(), KIND_GROUP, KIND_DIRECT, paired],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// Everyone we should address for this conversation — the active roster
    /// minus ourselves. One entry for a direct chat, N-1 for a group; the
    /// fan-out loop treats both the same.
    pub fn recipients(id: &[u8; 16]) -> Vec<[u8; 32]> {
        let me = Identity::get().map(|i| i.ipk());
        let conn = MESSAGES_DB.lock();
        Self::recipients_tx(&conn, id, me)
    }

    pub fn recipients_tx(conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>) -> Vec<[u8; 32]> {
        let me = me.unwrap_or([0u8; 32]);
        let Ok(mut stmt) = conn.prepare(
            "SELECT member_ipk FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk <> ?2 AND active = 1",
        ) else {
            return Vec::new();
        };
        stmt.query_map((id.as_slice(), me.as_slice()), |r| r.get::<_, Vec<u8>>(0))
            .map(|rows| rows.flatten().filter_map(|v| v.try_into().ok()).collect())
            .unwrap_or_default()
    }

    /// Full roster, including inactive members so past messages still
    /// attribute to someone who has since left.
    pub fn members(id: &[u8; 16]) -> Vec<MemberRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT * FROM conversation_members WHERE conversation_id = ?1 ORDER BY joined_at ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([id.as_slice()], MemberRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Add a member, or re-activate one who had left. Never demotes an
    /// existing role — a re-add must not strip an admin.
    pub fn put_member(conn: &Connection, id: &[u8; 16], member: &[u8; 32], role: u8) -> Result<()> {
        conn.execute(
            "INSERT INTO conversation_members (conversation_id, member_ipk, role, joined_at, active) \
             VALUES (?1, ?2, ?3, ?4, 1) \
             ON CONFLICT(conversation_id, member_ipk) DO UPDATE SET active = 1",
            (id.as_slice(), member.as_slice(), role, systime().as_secs()),
        )?;
        Ok(())
    }

    pub fn add_member(id: &[u8; 16], member: &[u8; 32], role: u8) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        Self::put_member(&conn, id, member, role)
    }

    /// Mark a member gone. The row survives so their past messages still
    /// resolve to a name.
    pub fn deactivate_member(id: &[u8; 16], member: &[u8; 32]) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        Self::deactivate_member_tx(&conn, id, member)
    }

    pub fn deactivate_member_tx(conn: &Connection, id: &[u8; 16], member: &[u8; 32]) -> Result<()> {
        conn.execute(
            "UPDATE conversation_members SET active = 0 \
             WHERE conversation_id = ?1 AND member_ipk = ?2",
            (id.as_slice(), member.as_slice()),
        )?;
        Ok(())
    }

    /// Replace the roster with `members`, marking anyone absent as departed,
    /// and take the roles and rules from the group's `meta`. The shape an
    /// applied MLS Commit hands us: the new membership, whole. A departed
    /// member keeps their row but no role, so being added back makes them an
    /// ordinary member again.
    pub fn sync_group(id: &[u8; 16], members: &[[u8; 32]], meta: Option<&GroupMeta>) -> Result<()> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE conversation_members SET active = 0 WHERE conversation_id = ?1",
            [id.as_slice()],
        )?;
        for m in members {
            Self::put_member(&tx, id, m, ROLE_MEMBER)?;
        }
        if let Some(meta) = meta {
            let state = meta.effective();
            tx.execute(
                "UPDATE conversation_members SET role = ?2 WHERE conversation_id = ?1",
                (id.as_slice(), ROLE_MEMBER),
            )?;
            for m in members {
                tx.execute(
                    "UPDATE conversation_members SET role = ?3 WHERE conversation_id = ?1 AND member_ipk = ?2",
                    (id.as_slice(), m.as_slice(), state.role(m)),
                )?;
            }
            let blob = meta.state.as_ref().map(postcard::to_allocvec).transpose()?;
            tx.execute(
                "UPDATE conversations SET group_state = ?2 WHERE id = ?1",
                (id.as_slice(), blob),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The rules a group runs by, as its signed state last said: read here, so
    /// a received message is checked without loading the MLS group. A group
    /// from before signed rules runs by its founder alone. `None` for a direct
    /// chat.
    pub fn state(id: &[u8; 16]) -> Option<GroupState> {
        let conn = MESSAGES_DB.lock();
        Self::state_tx(&conn, id)
    }

    pub fn state_tx(conn: &Connection, id: &[u8; 16]) -> Option<GroupState> {
        let (kind, blob, founder): (u8, Option<Vec<u8>>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT kind, group_state, created_by FROM conversations WHERE id = ?1",
                [id.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()?;
        if kind != KIND_GROUP {
            return None;
        }
        match blob {
            Some(blob) => postcard::from_bytes(&blob).ok(),
            None => {
                let meta = GroupMeta {
                    title:   String::new(),
                    founder: founder?.try_into().ok()?,
                    state:   None,
                };
                Some(meta.effective())
            },
        }
    }

    /// The group's rules are signed into it, rather than it running by its
    /// founder alone as groups from before them do.
    pub fn has_signed_rules(id: &[u8; 16]) -> bool {
        MESSAGES_DB
            .lock()
            .query_row(
                "SELECT group_state IS NOT NULL FROM conversations WHERE id = ?1",
                [id.as_slice()],
                |r| r.get(0),
            )
            .unwrap_or(false)
    }

    /// Who is in it now, us included.
    pub fn active_members(id: &[u8; 16]) -> Vec<[u8; 32]> {
        Self::members(id).into_iter().filter(|m| m.active).map(|m| m.member_ipk).collect()
    }

    /// Whether `member` may rename the group and change its photo: an admin, or
    /// anyone while its rules let members edit.
    pub fn may_edit(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = MESSAGES_DB.lock();
        Self::may_edit_tx(&conn, id, member)
    }

    pub fn may_edit_tx(conn: &Connection, id: &[u8; 16], member: &[u8; 32]) -> bool {
        let active = conn
            .query_row(
                "SELECT 1 FROM conversation_members WHERE conversation_id = ?1 AND member_ipk = ?2 AND active = 1",
                (id.as_slice(), member.as_slice()),
                |_| Ok(()),
            )
            .is_ok();
        active && Self::state_tx(conn, id).is_some_and(|s| s.may_edit(member))
    }

    /// Whether `member` runs this conversation *now*: an admin or an owner.
    ///
    /// Scoped to the active roster. A role row outlives its owner's membership
    /// so their old messages still attribute to a name, and reading that row as
    /// standing leaves someone who is out of the group still gating what the
    /// people in it may do.
    pub fn is_admin(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = MESSAGES_DB.lock();
        Self::is_admin_tx(&conn, id, member)
    }

    pub fn is_admin_tx(conn: &Connection, id: &[u8; 16], member: &[u8; 32]) -> bool {
        conn.query_row(
            "SELECT role FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk = ?2 AND active = 1",
            (id.as_slice(), member.as_slice()),
            |r| r.get::<_, i64>(0),
        )
        .map(|r| r as u8 >= ROLE_ADMIN)
        .unwrap_or(false)
    }

    /// Whether `member` is an owner of this group and still in it.
    pub fn is_owner(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = MESSAGES_DB.lock();
        Self::is_owner_tx(&conn, id, member)
    }

    pub fn is_owner_tx(conn: &Connection, id: &[u8; 16], member: &[u8; 32]) -> bool {
        conn.query_row(
            "SELECT 1 FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk = ?2 AND role = ?3 AND active = 1",
            (id.as_slice(), member.as_slice(), ROLE_OWNER),
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Capped like a peer's name: the title arrives from the wire, and a
    /// megabyte of it would be stored and drawn as-is.
    pub fn set_title(id: &[u8; 16], title: &str) -> Result<()> {
        let title: String = title.trim().chars().take(MAX_TITLE).collect();
        let conn = MESSAGES_DB.lock();
        conn.execute("UPDATE conversations SET title = ?1 WHERE id = ?2", (title, id.as_slice()))?;
        Ok(())
    }

    /// Every conversation, newest activity first — the home list.
    pub fn list() -> Vec<ConversationRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT c.* FROM conversations c \
             LEFT JOIN (SELECT conversation_id, MAX(id) AS last FROM messages WHERE deleted = 0 GROUP BY conversation_id) m \
               ON m.conversation_id = c.id \
             ORDER BY c.pinned DESC, COALESCE(m.last, '') DESC, c.created_at DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], ConversationRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Empty a conversation of its history, keeping the chat and its roster.
    pub fn clear_history(id: &[u8; 16]) -> Result<()> {
        let gid = Self::group_of(id);
        let _operation = gid.as_ref().map(|g| crate::mls::recovery::operation_lock(g).lock());
        let clear = crate::groups::recovery::clear_marker(id)?;
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        if let Some((key, through)) = clear {
            tx.execute(
                "INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,?2)",
                (key, through.to_string()),
            )?;
        }
        let orphaned = Self::clear_history_tx(&tx, id)?;
        tx.commit()?;
        crate::data::media::unlink_orphaned(&conn, &orphaned);
        drop(conn);
        crate::groups::recovery::finish_clears()
    }

    /// Every table scoped to a conversation, emptied — the shared half of
    /// clearing and deleting, so neither can forget one of them. Returns the
    /// `file_id`s whose rows it just dropped, for the caller to unlink *after*
    /// the transaction commits: a rollback must not leave the bytes gone.
    ///
    /// `seen_dispatch` is deliberately spared: it is keyed on the sender, not
    /// the conversation, and dropping its rows would let a redelivered dispatch
    /// be decrypted a second time — which the MLS ratchet answers with a hard
    /// SecretReuseError. `message_deletions` is also spared: clearing history
    /// must not allow a delayed, already-deleted post to reappear.
    pub fn clear_history_tx(conn: &Connection, id: &[u8; 16]) -> Result<Vec<[u8; 32]>> {
        // `message_media.file_id` is the only pointer at a received
        // attachment's bytes on disk, and the transfer store's GC reaps
        // FAILED/HELD partials alone — so these rows are the last chance to
        // know the files are there at all.
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file_id FROM message_media \
             WHERE conversation_id = ?1 AND file_id IS NOT NULL",
        )?;
        let orphaned: Vec<[u8; 32]> =
            stmt.query_map([id.as_slice()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        for table in [
            "messages",
            "reactions",
            "read_state",
            "member_read_state",
            "message_media",
            "attachment_sharing",
            "receipt_peers",
        ] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE conversation_id = ?1"),
                [id.as_slice()],
            )?;
        }
        Ok(orphaned)
    }

    /// Drop a conversation and everything scoped to it.
    pub fn delete(id: &[u8; 16]) -> Result<()> {
        let gid = Self::group_of(id);
        let _operation = gid.as_ref().map(|g| crate::mls::recovery::operation_lock(g).lock());
        let clear = crate::groups::recovery::clear_marker(id)?;
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        if let Some((key, through)) = clear {
            tx.execute(
                "INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,?2)",
                (key, through.to_string()),
            )?;
        }
        let orphaned = Self::clear_history_tx(&tx, id)?;
        tx.execute("DELETE FROM message_deletions WHERE conversation_id = ?1", [id.as_slice()])?;
        tx.execute(
            "DELETE FROM attachment_sharing_revocations WHERE conversation_id = ?1",
            [id.as_slice()],
        )?;
        tx.execute("DELETE FROM conversation_members WHERE conversation_id = ?1", [id.as_slice()])?;
        tx.execute("DELETE FROM conversations WHERE id = ?1", [id.as_slice()])?;
        tx.commit()?;
        crate::data::media::unlink_orphaned(&conn, &orphaned);
        drop(conn);
        crate::groups::recovery::finish_clears()
    }

    /// Flip a per-conversation flag. `column` is a fixed identifier, never
    /// caller-supplied — SQLite cannot bind a column name.
    fn set_flag(id: &[u8; 16], column: &'static str, on: bool) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            &format!("UPDATE conversations SET {column} = ?2 WHERE id = ?1"),
            (id.as_slice(), on),
        )?;
        Ok(())
    }

    pub fn set_pinned(id: &[u8; 16], on: bool) -> Result<()> {
        Self::set_flag(id, "pinned", on)
    }

    pub fn set_muted(id: &[u8; 16], on: bool) -> Result<()> {
        Self::set_flag(id, "muted", on)
    }

    /// Remember the newest message this chat has already alerted for.
    pub fn set_alerted_at(id: &[u8; 16], ts_secs: u64) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "UPDATE conversations SET alerted_at = ?2 WHERE id = ?1",
            (id.as_slice(), ts_secs),
        )?;
        Ok(())
    }

    /// Whether `who` is in any group we are also in.
    ///
    /// Co-membership is a relationship in its own right. A group's whole point
    /// is that people who have never paired can talk in it, so a sender we
    /// share a group with is expected mail even when they are nobody in our
    /// address book — the alternative is a group where two members silently
    /// cannot hear each other.
    ///
    /// Deliberately narrower than "we both exist": it is scoped to *active*
    /// membership, so leaving a group ends the standing it granted.
    pub fn shares_a_chat_with(who: &[u8; 32]) -> bool {
        let Some(me) = Identity::get().map(|i| i.ipk()) else { return false };
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT 1 FROM conversation_members mine \
             JOIN conversation_members theirs \
               ON theirs.conversation_id = mine.conversation_id \
             WHERE mine.member_ipk = ?1 AND mine.active = 1 \
               AND theirs.member_ipk = ?2 AND theirs.active = 1 LIMIT 1",
            (me.as_slice(), who.as_slice()),
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Every conversation and every member row, for the backup snapshot.
    ///
    /// Separate from [`Self::list`], which orders for the home screen and is
    /// free to filter later; a backup has to take the table as it stands.
    pub fn dump_all() -> (Vec<ConversationRow>, Vec<MemberRow>) {
        let conn = MESSAGES_DB.lock();
        let convs = conn
            .prepare("SELECT * FROM conversations")
            .and_then(|mut s| {
                s.query_map([], ConversationRow::from_row).map(|r| r.flatten().collect())
            })
            .unwrap_or_default();
        let members = conn
            .prepare("SELECT * FROM conversation_members")
            .and_then(|mut s| s.query_map([], MemberRow::from_row).map(|r| r.flatten().collect()))
            .unwrap_or_default();
        (convs, members)
    }

    /// Restore dumped conversations and rosters. `INSERT OR IGNORE`, so a
    /// conversation that already exists keeps whatever it has now — a blob is
    /// a snapshot of the past and must never overwrite the present.
    pub fn import_rows(convs: &[ConversationRow], members: &[MemberRow]) -> Result<usize> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let mut n = 0usize;
        for c in convs {
            n += tx.execute(
                "INSERT OR IGNORE INTO conversations \
                 (id, kind, title, mls_group_id, created_at, created_by, pinned, muted, alerted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                (
                    c.id.as_slice(),
                    c.kind,
                    &c.title,
                    c.mls_group_id.as_deref(),
                    c.created_at,
                    c.created_by.as_deref(),
                    c.pinned,
                    c.muted,
                    c.alerted_at,
                ),
            )?;
        }
        for m in members {
            tx.execute(
                "INSERT OR IGNORE INTO conversation_members \
                 (conversation_id, member_ipk, role, joined_at, active) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    m.conversation_id.as_slice(),
                    m.member_ipk.as_slice(),
                    m.role,
                    m.joined_at,
                    m.active,
                ),
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Parse a hex conversation id from the FFI boundary.
    pub fn id_from_bytes(bytes: &[u8]) -> Result<[u8; 16]> {
        bytes
            .try_into()
            .map_err(|_| anyhow!("conversation id must be 16 bytes, got {}", bytes.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;

    fn direct(conn: &Connection, peer: &[u8; 32], me: [u8; 32]) -> [u8; 16] {
        Conversation::for_peer_tx(conn, peer, Some(me)).expect("resolve direct")
    }

    #[test]
    fn activity_resolves_shared_groups_to_each_devices_local_chat() {
        let alice_db = open_in_memory();
        let bob_db = open_in_memory();
        let alice = [1u8; 32];
        let bob = [2u8; 32];
        let charlie = [3u8; 32];
        let dm_group = [4u8; 32];
        let shared_group = [5u8; 32];
        let alice_dm = direct(&alice_db, &bob, alice);
        let bob_dm = direct(&bob_db, &alice, bob);
        assert_ne!(alice_dm, bob_dm);
        Conversation::bind_group_tx(&alice_db, &alice_dm, &dm_group).unwrap();
        Conversation::bind_group_tx(&bob_db, &bob_dm, &dm_group).unwrap();
        let alice_chat =
            Conversation::join_group_tx(&alice_db, &alice, &[alice, bob, charlie]).unwrap();
        let bob_chat =
            Conversation::join_group_tx(&bob_db, &alice, &[alice, bob, charlie]).unwrap();
        assert_ne!(alice_chat, bob_chat);
        Conversation::bind_group_tx(&alice_db, &alice_chat, &shared_group).unwrap();
        Conversation::bind_group_tx(&bob_db, &bob_chat, &shared_group).unwrap();

        assert_eq!(Conversation::for_activity_tx(&bob_db, &dm_group, &alice), Some(bob_dm));
        assert_eq!(Conversation::for_activity_tx(&alice_db, &dm_group, &bob), Some(alice_dm));
        assert_eq!(Conversation::for_activity_tx(&bob_db, &shared_group, &alice), Some(bob_chat));
        assert_eq!(Conversation::for_activity_tx(&alice_db, &shared_group, &bob), Some(alice_chat));
        // A group-only member cannot target a DM or an unknown group.
        assert_eq!(Conversation::for_activity_tx(&bob_db, &dm_group, &charlie), None);
        assert_eq!(Conversation::for_activity_tx(&bob_db, &[9u8; 32], &alice), None);
        bob_db.execute(
            "UPDATE conversation_members SET active = 0 WHERE conversation_id = ?1 AND member_ipk = ?2",
            (bob_chat.as_slice(), alice.as_slice()),
        ).unwrap();
        assert_eq!(Conversation::for_activity_tx(&bob_db, &shared_group, &alice), None);
        // Re-pairing invalidates the old shared address without changing the local chat.
        Conversation::bind_group_tx(&bob_db, &bob_dm, &[6u8; 32]).unwrap();
        assert_eq!(Conversation::for_activity_tx(&bob_db, &dm_group, &alice), None);
        assert_eq!(Conversation::for_activity_tx(&bob_db, &[6u8; 32], &alice), Some(bob_dm));
    }

    #[test]
    fn transport_uses_only_existing_active_encrypted_chats() {
        let conn = open_in_memory();
        let me = [11; 32];
        let peer = [12; 32];
        let admin = [13; 32];
        let resolve = |paired| Conversation::for_peer_transport_tx(&conn, &me, &peer, paired);
        assert_eq!(resolve(false), None);
        let dm = direct(&conn, &peer, me);
        assert_eq!(resolve(true), None, "a chat without MLS cannot carry signaling");
        Conversation::bind_group_tx(&conn, &dm, &[14; 32]).unwrap();
        assert_eq!(resolve(false), None, "a stale DM does not grant unpaired access");
        assert_eq!(resolve(true), Some(dm));
        let group = Conversation::join_group_tx(&conn, &admin, &[admin, me, peer]).unwrap();
        assert_eq!(resolve(false), None, "group must have an encrypted channel");
        Conversation::bind_group_tx(&conn, &group, &[15; 32]).unwrap();
        assert_eq!(resolve(false), Some(group));
        assert_eq!(resolve(true), Some(dm), "prefer a paired private channel");
        conn.execute(
            "DELETE FROM conversation_members WHERE conversation_id = ?1 AND member_ipk = ?2",
            (dm.as_slice(), me.as_slice()),
        )
        .unwrap();
        assert_eq!(resolve(true), Some(dm), "legacy direct chats may omit our roster row");
        Conversation::deactivate_member_tx(&conn, &group, &peer).unwrap();
        assert_eq!(resolve(false), None, "removed peer loses transport permission");
        Conversation::put_member(&conn, &group, &peer, ROLE_MEMBER).unwrap();
        Conversation::deactivate_member_tx(&conn, &group, &me).unwrap();
        assert_eq!(resolve(false), None, "leaving ends our transport permission too");
        assert_eq!(Conversation::for_peer_transport_tx(&conn, &me, &me, true), None);
    }

    /// `for_peer` is find-or-create: the second call for the same peer must
    /// return the first conversation, not mint a second one.
    #[test]
    fn direct_conversation_resolves_once_per_peer() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let peer = [2u8; 32];

        let a = direct(&conn, &peer, me);
        let b = direct(&conn, &peer, me);
        assert_eq!(a, b, "same peer must resolve to the same conversation");

        let other = direct(&conn, &[3u8; 32], me);
        assert_ne!(a, other, "a different peer gets its own conversation");

        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0)).expect("count");
        assert_eq!(n, 2, "exactly two conversations minted");
    }

    /// Both parties land in the roster, and `recipients` excludes us — that
    /// exclusion is what makes one fan-out loop serve 1:1 and groups alike.
    #[test]
    fn roster_holds_both_parties_and_recipients_drops_self() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let peer = [2u8; 32];
        let id = direct(&conn, &peer, me);

        assert_eq!(Conversation::peer_of_tx(&conn, &id, Some(me)), Some(peer));
        assert_eq!(Conversation::recipients_tx(&conn, &id, Some(me)), vec![peer]);
    }

    /// The point of the whole design: re-pointing a conversation at a freshly
    /// minted MLS group must not disturb its history. Mirrors what the three
    /// heal paths do after a restore or a re-pair.
    #[test]
    fn rebinding_the_mls_group_keeps_history() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let peer = [2u8; 32];
        let id = direct(&conn, &peer, me);

        conn.execute(
            "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status) \
             VALUES ('01H', ?1, 'before the restore', 0, 100, 1)",
            [id.as_slice()],
        )
        .unwrap();

        Conversation::bind_group_tx(&conn, &id, &[0xAA; 32]).expect("bind");
        assert_eq!(Conversation::for_group_tx(&conn, &[0xAA; 32]), Some(id));

        // The group died and was re-established under a new id.
        Conversation::bind_group_tx(&conn, &id, &[0xBB; 32]).expect("rebind");
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(id));
        assert_eq!(Conversation::for_group_tx(&conn, &[0xAA; 32]), None, "old pointer released");

        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                [id.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "history survives the group rotation");
    }

    /// Being Welcomed into a group must not disturb the direct chat with
    /// whoever sent the Welcome. Filing the group under their DM — which is
    /// what happens if the joiner resolves by sender instead of by roster —
    /// puts every group message in that DM and points it at a group's keys.
    #[test]
    fn joining_a_group_leaves_the_inviters_dm_alone() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let inviter = [2u8; 32];
        let third = [3u8; 32];

        let dm = direct(&conn, &inviter, me);
        Conversation::bind_group_tx(&conn, &dm, &[0xAA; 32]).expect("bind the pair");

        let group =
            Conversation::join_group_tx(&conn, &inviter, &[me, inviter, third]).expect("join");
        Conversation::bind_group_tx(&conn, &group, &[0xBB; 32]).expect("bind the group");

        assert_ne!(group, dm, "a group is not the inviter's direct chat");
        assert_eq!(
            Conversation::for_group_tx(&conn, &[0xAA; 32]),
            Some(dm),
            "the DM keeps its group"
        );
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(group));

        // The inviter is the admin; we are an ordinary member and are in the
        // roster exactly once despite also being in the MLS member list.
        assert!(Conversation::is_admin_tx(&conn, &group, &inviter), "the inviter admins it");
        assert!(!Conversation::is_admin_tx(&conn, &group, &me));
        assert_eq!(Conversation::recipients_tx(&conn, &group, Some(me)), vec![inviter, third]);
    }

    /// A group of three where two members have never paired is the ordinary
    /// case, not an edge one — so co-membership has to grant standing, or each
    /// of them can hear whoever invited them and not the other.
    #[test]
    fn sharing_a_group_is_standing_enough_to_be_heard() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let stranger = [3u8; 32];

        let shares = |who: &[u8; 32]| {
            conn.query_row(
                "SELECT 1 FROM conversation_members mine \
                 JOIN conversation_members theirs \
                   ON theirs.conversation_id = mine.conversation_id \
                 WHERE mine.member_ipk = ?1 AND mine.active = 1 \
                   AND theirs.member_ipk = ?2 AND theirs.active = 1 LIMIT 1",
                (me.as_slice(), who.as_slice()),
                |_| Ok(()),
            )
            .is_ok()
        };

        assert!(!shares(&stranger), "nobody shares anything yet");

        let group = Conversation::join_group_tx(&conn, &[2u8; 32], &[me, [2u8; 32], stranger])
            .expect("join");
        assert!(shares(&stranger), "a group we are both in is standing");

        // Leaving ends it: the row survives so old messages still attribute,
        // but it stops being a licence to reach us.
        Conversation::deactivate_member_tx(&conn, &group, &stranger).expect("deactivate");
        assert!(!shares(&stranger), "a member who left keeps no standing");
    }

    /// A group id backs at most one conversation. When a re-pair adopts a
    /// group another conversation still claims, the stale pointer is released
    /// rather than colliding on the unique index.
    #[test]
    fn binding_a_claimed_group_evicts_the_previous_holder() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let first = direct(&conn, &[2u8; 32], me);
        let second = direct(&conn, &[3u8; 32], me);
        let gid = [0xCC; 32];

        Conversation::bind_group_tx(&conn, &first, &gid).expect("bind first");
        Conversation::bind_group_tx(&conn, &second, &gid).expect("bind second");

        assert_eq!(Conversation::for_group_tx(&conn, &gid), Some(second));
        assert!(
            Conversation::get_tx(&conn, &first).unwrap().mls_group_id.is_none(),
            "evicted conversation keeps its rows but loses the pointer"
        );
    }

    /// Clearing is emptying, not deleting: the chat has to still be there to
    /// keep talking in afterwards, roster and all.
    #[test]
    fn clearing_history_empties_the_messages_and_keeps_the_chat() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let group = Conversation::join_group_tx(&conn, &[2u8; 32], &[me, [2u8; 32], [3u8; 32]])
            .expect("join");

        conn.execute(
            "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status) \
             VALUES ('01H', ?1, 'said something', 0, 100, 1)",
            [group.as_slice()],
        )
        .unwrap();

        Conversation::clear_history_tx(&conn, &group).expect("clear");

        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                [group.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "the history is gone");
        assert!(Conversation::get_tx(&conn, &group).is_some(), "the chat itself survives");
        assert_eq!(
            Conversation::recipients_tx(&conn, &group, Some(me)).len(),
            2,
            "and so does everyone in it"
        );
    }

    /// A media row is the only pointer at the attachment's bytes on disk, so a
    /// clear that doesn't name what it orphans leaves the file there with
    /// nothing left that knows it exists.
    #[test]
    fn clearing_history_hands_back_the_attachments_it_orphans() {
        let conn = open_in_memory();
        let group =
            Conversation::join_group_tx(&conn, &[2u8; 32], &[[1u8; 32], [2u8; 32]]).expect("join");
        let file = [0xab; 32];

        conn.execute(
            "INSERT INTO message_media (conversation_id, dispatch_id, kind, mime, file_id) \
             VALUES (?1, X'01', 0, 'image/png', ?2)",
            (group.as_slice(), file.as_slice()),
        )
        .unwrap();

        assert_eq!(
            Conversation::clear_history_tx(&conn, &group).expect("clear"),
            vec![file],
            "the file_id comes back for the caller to unlink"
        );
        assert!(
            Conversation::clear_history_tx(&conn, &group).expect("clear again").is_empty(),
            "and only while a row still points at it"
        );
    }
}
