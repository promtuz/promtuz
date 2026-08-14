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
use crate::utils::systime;

/// Two-party chat, titled by the peer's contact name.
pub const KIND_DIRECT: u8 = 0;
/// Multi-member chat, carrying its own title and roster.
pub const KIND_GROUP: u8 = 1;

/// Ordinary member: may speak and may leave.
pub const ROLE_MEMBER: u8 = 0;
/// Admin: may also add and remove. v1 mints exactly one, the creator.
pub const ROLE_ADMIN: u8 = 1;

/// Time-sortable, so an unordered conversation list still reads oldest-first.
fn mint_conversation_id() -> [u8; 16] {
    Ulid::new().to_bytes()
}

pub struct Conversation;

impl Conversation {
    pub fn get(id: &[u8; 16]) -> Option<ConversationRow> {
        let conn = MESSAGES_DB.lock();
        Self::get_tx(&conn, id)
    }

    pub fn get_tx(conn: &Connection, id: &[u8; 16]) -> Option<ConversationRow> {
        conn.query_row("SELECT * FROM conversations WHERE id = ?1", [id.as_slice()], ConversationRow::from_row)
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

    /// Create a group conversation with us as admin and `members` as the
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
            Self::put_member(&conn, &id, &me, ROLE_ADMIN)?;
        }
        for m in members {
            Self::put_member(&conn, &id, m, ROLE_MEMBER)?;
        }
        Ok(id)
    }

    /// Create a group conversation we were Welcomed into, rather than founded.
    ///
    /// The roster comes from the MLS group itself — it is the authority on who
    /// is in it, and it already includes us. `creator` is the member who sent
    /// the Welcome and is recorded as the admin; the title arrives separately,
    /// so a group can exist unnamed for a moment.
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
        Self::put_member(&conn, &id, creator, ROLE_ADMIN)?;
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

    pub fn peer_of_tx(
        conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>,
    ) -> Option<[u8; 32]> {
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

    /// Everyone we should address for this conversation — the active roster
    /// minus ourselves. One entry for a direct chat, N-1 for a group; the
    /// fan-out loop treats both the same.
    pub fn recipients(id: &[u8; 16]) -> Vec<[u8; 32]> {
        let me = Identity::get().map(|i| i.ipk());
        let conn = MESSAGES_DB.lock();
        Self::recipients_tx(&conn, id, me)
    }

    pub fn recipients_tx(
        conn: &Connection, id: &[u8; 16], me: Option<[u8; 32]>,
    ) -> Vec<[u8; 32]> {
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
    pub fn put_member(
        conn: &Connection, id: &[u8; 16], member: &[u8; 32], role: u8,
    ) -> Result<()> {
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
        conn.execute(
            "UPDATE conversation_members SET active = 0 \
             WHERE conversation_id = ?1 AND member_ipk = ?2",
            (id.as_slice(), member.as_slice()),
        )?;
        Ok(())
    }

    /// Replace the roster with `members`, marking anyone absent as departed.
    /// The shape an applied MLS Commit hands us: the new membership, whole.
    pub fn sync_roster(id: &[u8; 16], members: &[[u8; 32]]) -> Result<()> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE conversation_members SET active = 0 WHERE conversation_id = ?1",
            [id.as_slice()],
        )?;
        for m in members {
            Self::put_member(&tx, id, m, ROLE_MEMBER)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn is_admin(id: &[u8; 16], member: &[u8; 32]) -> bool {
        let conn = MESSAGES_DB.lock();
        Self::is_admin_tx(&conn, id, member)
    }

    pub fn is_admin_tx(conn: &Connection, id: &[u8; 16], member: &[u8; 32]) -> bool {
        conn.query_row(
            "SELECT role FROM conversation_members WHERE conversation_id = ?1 AND member_ipk = ?2",
            (id.as_slice(), member.as_slice()),
            |r| r.get::<_, i64>(0),
        )
        .map(|r| r as u8 == ROLE_ADMIN)
        .unwrap_or(false)
    }

    pub fn set_title(id: &[u8; 16], title: &str) -> Result<()> {
        let conn = MESSAGES_DB.lock();
        conn.execute("UPDATE conversations SET title = ?1 WHERE id = ?2", (title, id.as_slice()))?;
        Ok(())
    }

    /// Every conversation, newest activity first — the home list.
    pub fn list() -> Vec<ConversationRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT c.* FROM conversations c \
             LEFT JOIN (SELECT conversation_id, MAX(id) AS last FROM messages GROUP BY conversation_id) m \
               ON m.conversation_id = c.id \
             ORDER BY COALESCE(m.last, '') DESC, c.created_at DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], ConversationRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Drop a conversation and everything scoped to it.
    pub fn delete(id: &[u8; 16]) -> Result<()> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        // `seen_dispatch` is deliberately spared: it is keyed on the sender,
        // not the conversation, and dropping its rows would let a redelivered
        // dispatch be decrypted a second time — which the MLS ratchet answers
        // with a hard SecretReuseError.
        for table in
            ["messages", "reactions", "read_state", "member_read_state", "message_media"]
        {
            tx.execute(&format!("DELETE FROM {table} WHERE conversation_id = ?1"), [id.as_slice()])?;
        }
        tx.execute("DELETE FROM conversation_members WHERE conversation_id = ?1", [id.as_slice()])?;
        tx.execute("DELETE FROM conversations WHERE id = ?1", [id.as_slice()])?;
        tx.commit()?;
        Ok(())
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
                 (id, kind, title, mls_group_id, created_at, created_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    c.id.as_slice(),
                    c.kind,
                    &c.title,
                    c.mls_group_id.as_deref(),
                    c.created_at,
                    c.created_by.as_deref(),
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
        bytes.try_into().map_err(|_| anyhow!("conversation id must be 16 bytes, got {}", bytes.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;

    fn direct(conn: &Connection, peer: &[u8; 32], me: [u8; 32]) -> [u8; 16] {
        Conversation::for_peer_tx(conn, peer, Some(me)).expect("resolve direct")
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

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .expect("count");
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
            .query_row("SELECT COUNT(*) FROM messages WHERE conversation_id = ?1", [id.as_slice()], |r| {
                r.get(0)
            })
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

        let group = Conversation::join_group_tx(&conn, &inviter, &[me, inviter, third])
            .expect("join");
        Conversation::bind_group_tx(&conn, &group, &[0xBB; 32]).expect("bind the group");

        assert_ne!(group, dm, "a group is not the inviter's direct chat");
        assert_eq!(Conversation::for_group_tx(&conn, &[0xAA; 32]), Some(dm), "the DM keeps its group");
        assert_eq!(Conversation::for_group_tx(&conn, &[0xBB; 32]), Some(group));

        // The inviter is the admin; we are an ordinary member and are in the
        // roster exactly once despite also being in the MLS member list.
        assert!(Conversation::is_admin_tx(&conn, &group, &inviter), "the inviter admins it");
        assert!(!Conversation::is_admin_tx(&conn, &group, &me));
        assert_eq!(Conversation::recipients_tx(&conn, &group, Some(me)), vec![inviter, third]);
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
}
