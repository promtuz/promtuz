//! Conversations: a local 16-byte id that never changes, pointing at the MLS group backing it now.
//! Group ids get re-minted under a live chat, so nothing keys history on them.

use anyhow::Result;
use common::utils::now_secs;
use rusqlite::Connection;
use ulid::Ulid;

use crate::data::identity::Identity;
use crate::db::all;
use crate::db::messages::ConversationRow;
use crate::db::messages::MemberRow;
use crate::mls::GroupMeta;
use crate::mls::GroupState;
use crate::state::core;

pub const KIND_DIRECT: u8 = 0;
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
        let conn = core().db.messages().lock();
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

    pub fn for_peer(peer: &[u8; 32]) -> Result<[u8; 16]> {
        // Never take the identity lock while holding the messages lock.
        let me = Identity::local_ipk();
        let conn = core().db.messages().lock();
        Self::for_peer_tx(&conn, peer, me)
    }

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
        let now = now_secs();
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
                |r| r.get(0),
            )
            .ok();
        Ok(found)
    }

    pub fn create_group(title: &str, members: &[[u8; 32]]) -> Result<[u8; 16]> {
        let me = Identity::local_ipk();
        let conn = core().db.messages().lock();
        let id = mint_conversation_id();
        let now = now_secs();
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

    /// `creator` starts as owner until [`Self::sync_group`] reads the roles off the MLS group.
    pub fn join_group(creator: &[u8; 32], members: &[[u8; 32]]) -> Result<[u8; 16]> {
        let conn = core().db.messages().lock();
        Self::join_group_tx(&conn, creator, members)
    }

    pub fn join_group_tx(
        conn: &Connection, creator: &[u8; 32], members: &[[u8; 32]],
    ) -> Result<[u8; 16]> {
        let id = mint_conversation_id();
        let now = now_secs();
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

    pub fn for_group(group_id: &[u8; 32]) -> Option<[u8; 16]> {
        let conn = core().db.messages().lock();
        Self::for_group_tx(&conn, group_id)
    }

    pub fn for_group_tx(conn: &Connection, group_id: &[u8; 32]) -> Option<[u8; 16]> {
        conn.query_row(
            "SELECT id FROM conversations WHERE mls_group_id = ?1",
            [group_id.as_slice()],
            |r| r.get(0),
        )
        .ok()
    }

    pub(crate) fn for_activity(group_id: &[u8; 32], sender: &[u8; 32]) -> Option<[u8; 16]> {
        Self::for_activity_tx(&core().db.messages().lock(), group_id, sender)
    }

    fn for_activity_tx(
        conn: &Connection, group_id: &[u8; 32], sender: &[u8; 32],
    ) -> Option<[u8; 16]> {
        conn.query_row(
            "SELECT c.id FROM conversations c \
             JOIN conversation_members m ON m.conversation_id = c.id \
             WHERE c.mls_group_id = ?1 AND m.member_ipk = ?2 AND m.active = 1",
            (group_id.as_slice(), sender.as_slice()),
            |r| r.get(0),
        )
        .ok()
    }

    /// A group backs at most one conversation, so binding evicts any stale pointer to it first.
    pub fn bind_group(id: &[u8; 16], group_id: &[u8; 32]) -> Result<()> {
        let conn = core().db.messages().lock();
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
        core().db.messages().lock().execute(
            "UPDATE conversations SET mls_group_id = NULL WHERE id = ?1",
            [id.as_slice()],
        )?;
        Ok(())
    }

    pub fn group_of(id: &[u8; 16]) -> Option<[u8; 32]> {
        Self::get(id).and_then(|c| c.mls_group_id).and_then(|v| v.try_into().ok())
    }

    /// Any member but us, even in a group, so callers check that the chat is direct.
    pub fn peer_of(id: &[u8; 16]) -> Option<[u8; 32]> {
        let me = Identity::local_ipk();
        let conn = core().db.messages().lock();
        Self::peer_of_tx(&conn, id, me)
    }

    pub fn peer_of_tx(conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>) -> Option<[u8; 32]> {
        let me = me.unwrap_or([0u8; 32]);
        conn.query_row(
            "SELECT member_ipk FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk <> ?2 LIMIT 1",
            (id.as_slice(), me.as_slice()),
            |r| r.get(0),
        )
        .ok()
    }

    /// Never creates a chat. Unpaired peers may use only a group we are both still active in.
    pub(crate) fn for_peer_transport(peer: &[u8; 32], paired: bool) -> Option<[u8; 16]> {
        let me = Identity::local_ipk()?;
        Self::for_peer_transport_tx(&core().db.messages().lock(), &me, peer, paired)
    }

    pub(crate) fn for_peer_transport_tx(
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
            |r| r.get(0),
        )
        .ok()
    }

    pub fn recipients(id: &[u8; 16]) -> Vec<[u8; 32]> {
        let me = Identity::local_ipk();
        let conn = core().db.messages().lock();
        Self::recipients_tx(&conn, id, me).unwrap_or_default()
    }

    pub fn recipients_tx(
        conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>,
    ) -> rusqlite::Result<Vec<[u8; 32]>> {
        all(
            conn,
            "SELECT member_ipk FROM conversation_members \
             WHERE conversation_id = ?1 AND member_ipk <> ?2 AND active = 1",
            (id.as_slice(), me.unwrap_or([0u8; 32]).as_slice()),
            |r| r.get(0),
        )
    }

    pub fn members(id: &[u8; 16]) -> Vec<MemberRow> {
        all(
            &core().db.messages().lock(),
            "SELECT * FROM conversation_members WHERE conversation_id = ?1 ORDER BY joined_at ASC",
            [id.as_slice()],
            MemberRow::from_row,
        )
        .unwrap_or_default()
    }

    /// Never changes an existing role: a re-add must not strip an admin.
    pub fn put_member(conn: &Connection, id: &[u8; 16], member: &[u8; 32], role: u8) -> Result<()> {
        conn.execute(
            "INSERT INTO conversation_members (conversation_id, member_ipk, role, joined_at, active) \
             VALUES (?1, ?2, ?3, ?4, 1) \
             ON CONFLICT(conversation_id, member_ipk) DO UPDATE SET active = 1",
            (id.as_slice(), member.as_slice(), role, now_secs()),
        )?;
        Ok(())
    }

    /// The row survives so their past messages still resolve to a name.
    pub fn deactivate_member(id: &[u8; 16], member: &[u8; 32]) -> Result<()> {
        let conn = core().db.messages().lock();
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

    pub fn sync_group(id: &[u8; 16], members: &[[u8; 32]], meta: Option<&GroupMeta>) -> Result<()> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        Self::sync_group_tx(&tx, id, members, meta)?;
        tx.commit()?;
        Ok(())
    }

    fn sync_group_tx(tx: &Connection, id: &[u8; 16], members: &[[u8; 32]], meta: Option<&GroupMeta>) -> Result<()> {
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
        Ok(())
    }

    /// Replace encryption without replacing the conversation or its content.
    pub(crate) fn migrate_group(
        id: &[u8; 16], old: &[u8; 32], target: &[u8; 32], members: &[[u8; 32]],
        meta: &GroupMeta, anchor: [u8; 32],
    ) -> Result<()> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        let changed = tx.execute("UPDATE conversations SET mls_group_id=?3 WHERE id=?1 AND kind=1 AND (mls_group_id=?2 OR mls_group_id=?3)",
            (id.as_slice(),old.as_slice(),target.as_slice()))?;
        anyhow::ensure!(changed == 1, "migration conversation changed");
        Self::sync_group_tx(&tx, id, members, Some(meta))?;
        for (key, value) in [
            (format!("group_migrated:{}",hex::encode(old)),hex::encode(target)),
            (format!("group_anchor:{}",hex::encode(target)),hex::encode(anchor)),
        ] {
            tx.execute("INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,?2)", (key,value))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A group from before signed rules runs by its founder alone.
    pub fn state(id: &[u8; 16]) -> Option<GroupState> {
        let conn = core().db.messages().lock();
        Self::state_tx(&conn, id)
    }

    pub fn state_tx(conn: &Connection, id: &[u8; 16]) -> Option<GroupState> {
        let (kind, blob, founder): (u8, Option<Vec<u8>>, Option<[u8; 32]>) = conn
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
                    founder: founder?,
                    state:   None,
                };
                Some(meta.effective())
            },
        }
    }

    pub fn has_signed_rules(id: &[u8; 16]) -> bool {
        core().db.messages()
            .lock()
            .query_row(
                "SELECT group_state IS NOT NULL FROM conversations WHERE id = ?1",
                [id.as_slice()],
                |r| r.get(0),
            )
            .unwrap_or(false)
    }

    pub fn active_members(id: &[u8; 16]) -> Vec<[u8; 32]> {
        Self::members(id).into_iter().filter(|m| m.active).map(|m| m.member_ipk).collect()
    }

    pub fn may_edit(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = core().db.messages().lock();
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

    pub fn is_owner(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = core().db.messages().lock();
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

    /// Capped because the title arrives from the wire.
    pub fn set_title(id: &[u8; 16], title: &str) -> Result<()> {
        let title: String = title.trim().chars().take(MAX_TITLE).collect();
        let conn = core().db.messages().lock();
        conn.execute("UPDATE conversations SET title = ?1 WHERE id = ?2", (title, id.as_slice()))?;
        Ok(())
    }

    pub fn list() -> Vec<ConversationRow> {
        all(
            &core().db.messages().lock(),
            "SELECT c.* FROM conversations c \
             ORDER BY c.pinned DESC, \
               COALESCE((SELECT id FROM messages m WHERE m.conversation_id = c.id AND m.deleted = 0 \
                         ORDER BY m.id DESC LIMIT 1), '') DESC, \
               c.created_at DESC",
            [],
            ConversationRow::from_row,
        )
        .unwrap_or_default()
    }

    pub fn clear_history(id: &[u8; 16]) -> Result<()> {
        let gid = Self::group_of(id);
        let _operation = gid.as_ref().map(|g| crate::mls::recovery::operation_lock(g).lock());
        let clear = crate::groups::recovery::clear_marker(id)?;
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        if let Some((key, through)) = clear {
            tx.execute(
                "INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,?2)",
                (key, through.to_string()),
            )?;
        }
        let orphaned = Self::clear_history_tx(&tx, id)?;
        tx.commit()?;
        crate::data::media::unlink_orphaned(&core().db, &conn, &orphaned);
        drop(conn);
        crate::groups::recovery::finish_clears()
    }

    /// Returns the dropped `file_id`s, to unlink only after commit. `seen_dispatch` and
    /// `message_deletions` survive: a redelivery must not decrypt twice, nor a deleted post return.
    pub fn clear_history_tx(conn: &Connection, id: &[u8; 16]) -> Result<Vec<[u8; 32]>> {
        // These rows are the only pointer at a received attachment's bytes, and the transfer GC
        // reaps only failed or held partials.
        let orphaned: Vec<[u8; 32]> = all(
            conn,
            "SELECT DISTINCT file_id FROM message_media \
             WHERE conversation_id = ?1 AND file_id IS NOT NULL",
            [id.as_slice()],
            |r| r.get(0),
        )?;

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

    pub fn delete(id: &[u8; 16]) -> Result<()> {
        let gid = Self::group_of(id);
        let _operation = gid.as_ref().map(|g| crate::mls::recovery::operation_lock(g).lock());
        let clear = crate::groups::recovery::clear_marker(id)?;
        let mut conn = core().db.messages().lock();
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
        crate::data::media::unlink_orphaned(&core().db, &conn, &orphaned);
        drop(conn);
        crate::groups::recovery::finish_clears()
    }

    /// SQLite cannot bind a column name, so `column` must stay a fixed identifier.
    fn set_flag(id: &[u8; 16], column: &'static str, on: bool) -> Result<()> {
        let conn = core().db.messages().lock();
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

    pub fn set_alerted_at(id: &[u8; 16], ts_secs: u64) -> Result<()> {
        let conn = core().db.messages().lock();
        conn.execute(
            "UPDATE conversations SET alerted_at = ?2 WHERE id = ?1",
            (id.as_slice(), ts_secs),
        )?;
        Ok(())
    }

    /// Active co-membership makes a sender expected mail even outside the address book.
    pub fn shares_a_chat_with(who: &[u8; 32]) -> bool {
        let Some(me) = Identity::local_ipk() else { return false };
        let conn = core().db.messages().lock();
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

    pub fn dump_all_tx(
        conn: &Connection,
    ) -> rusqlite::Result<(Vec<ConversationRow>, Vec<MemberRow>)> {
        Ok((
            all(conn, "SELECT * FROM conversations", [], ConversationRow::from_row)?,
            all(conn, "SELECT * FROM conversation_members", [], MemberRow::from_row)?,
        ))
    }

    pub fn import_rows_tx(
        conn: &Connection, convs: &[ConversationRow], members: &[MemberRow],
    ) -> Result<usize> {
        let mut n = 0usize;
        for c in convs {
            n += conn.execute(
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
            conn.execute(
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
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::media::MediaRow;
    use crate::data::message::Message;
    use crate::test_support::data::open;

    const ME: [u8; 32] = [1; 32];

    fn direct(conn: &Connection, peer: &[u8; 32]) -> [u8; 16] {
        Conversation::for_peer_tx(conn, peer, Some(ME)).unwrap()
    }

    /// The heal paths after a restore or a re-pair point a chat at a fresh MLS group; its history
    /// stays, and a group id another chat still claims moves rather than colliding.
    #[test]
    fn rebinding_the_mls_group_keeps_history_and_evicts_the_previous_holder() {
        let conn = open(crate::db::messages::migrate);
        let peer = [2u8; 32];
        let id = direct(&conn, &peer);
        Message::save_incoming_tx(&conn, id, peer, &[1; 16], "before the restore", 100, None)
            .unwrap();

        Conversation::bind_group_tx(&conn, &id, &[0xAA; 32]).unwrap();
        Conversation::bind_group_tx(&conn, &id, &[0xBB; 32]).unwrap();
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(id));
        assert_eq!(Conversation::for_group_tx(&conn, &[0xAA; 32]), None, "old pointer released");
        assert_eq!(Message::get_messages_tx(&conn, &id, 10, "").unwrap().len(), 1);

        let other = direct(&conn, &[3; 32]);
        Conversation::bind_group_tx(&conn, &other, &[0xBB; 32]).unwrap();
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(other));
        let evicted = Conversation::get_tx(&conn, &id).unwrap();
        assert_eq!(evicted.mls_group_id, None, "the evicted chat keeps its rows, not the pointer");
        assert_eq!(Message::get_messages_tx(&conn, &id, 10, "").unwrap().len(), 1);
    }

    /// Resolving a Welcome by its sender would file the group under the inviter's DM and point
    /// that DM at the group's keys.
    #[test]
    fn joining_a_group_leaves_the_inviters_dm_alone() {
        let conn = open(crate::db::messages::migrate);
        let (inviter, third) = ([2u8; 32], [3u8; 32]);
        let dm = direct(&conn, &inviter);
        Conversation::bind_group_tx(&conn, &dm, &[0xAA; 32]).unwrap();

        let group = Conversation::join_group_tx(&conn, &inviter, &[ME, inviter, third]).unwrap();
        Conversation::bind_group_tx(&conn, &group, &[0xBB; 32]).unwrap();

        assert_ne!(group, dm);
        assert_eq!(
            Conversation::for_group_tx(&conn, &[0xAA; 32]),
            Some(dm),
            "the DM keeps its group"
        );
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(group));
        assert!(Conversation::is_owner_tx(&conn, &group, &inviter));
        assert!(!Conversation::is_owner_tx(&conn, &group, &ME));
        assert_eq!(
            Conversation::recipients_tx(&conn, &group, Some(ME)).unwrap(),
            vec![inviter, third]
        );
    }

    #[test]
    fn transport_uses_only_existing_active_encrypted_chats() {
        let conn = open(crate::db::messages::migrate);
        let (peer, admin) = ([12u8; 32], [13u8; 32]);
        let resolve = |paired| Conversation::for_peer_transport_tx(&conn, &ME, &peer, paired);
        assert_eq!(resolve(false), None);
        let dm = direct(&conn, &peer);
        assert_eq!(resolve(true), None, "a chat without MLS cannot carry signaling");
        Conversation::bind_group_tx(&conn, &dm, &[14; 32]).unwrap();
        assert_eq!(resolve(false), None, "a stale DM does not grant unpaired access");
        assert_eq!(resolve(true), Some(dm));
        let group = Conversation::join_group_tx(&conn, &admin, &[admin, ME, peer]).unwrap();
        assert_eq!(resolve(false), None, "the group needs an encrypted channel");
        Conversation::bind_group_tx(&conn, &group, &[15; 32]).unwrap();
        assert_eq!(resolve(false), Some(group));
        assert_eq!(resolve(true), Some(dm), "a paired private channel comes first");
        // Direct chats migrated from before rosters hold only the peer's row.
        conn.execute(
            "DELETE FROM conversation_members WHERE conversation_id = ?1 AND member_ipk = ?2",
            (dm.as_slice(), ME.as_slice()),
        )
        .unwrap();
        assert_eq!(resolve(true), Some(dm));
        Conversation::deactivate_member_tx(&conn, &group, &peer).unwrap();
        assert_eq!(resolve(false), None, "a removed peer loses transport");
        Conversation::put_member(&conn, &group, &peer, ROLE_MEMBER).unwrap();
        Conversation::deactivate_member_tx(&conn, &group, &ME).unwrap();
        assert_eq!(resolve(false), None, "leaving ends our transport too");
        assert_eq!(Conversation::for_peer_transport_tx(&conn, &ME, &ME, true), None);
    }

    /// A media row is the only pointer at an attachment's bytes, so clearing returns the files it
    /// orphans for the caller to unlink after commit.
    #[test]
    fn clearing_history_hands_back_the_attachments_it_orphans() {
        let conn = open(crate::db::messages::migrate);
        let group = Conversation::join_group_tx(&conn, &[2; 32], &[ME, [2; 32]]).unwrap();
        let file = [0xAB; 32];
        let media = MediaRow {
            kind:        crate::data::media::KIND_ATTACHMENT,
            group_id:    None,
            mime:        "image/png".into(),
            name:        String::new(),
            size:        0,
            width:       0,
            height:      0,
            duration_ms: 0,
            blob:        None,
            thumb:       None,
            file_id:     Some(file.to_vec()),
            sticker:     None,
        };
        crate::data::media::save_tx(&conn, &group, &[1; 16], &media).unwrap();

        assert_eq!(Conversation::clear_history_tx(&conn, &group).unwrap(), vec![file]);
        assert!(Conversation::clear_history_tx(&conn, &group).unwrap().is_empty());
        assert!(Conversation::get_tx(&conn, &group).is_some(), "clearing keeps the chat");
    }
}
