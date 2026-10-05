use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;
use serde::Deserialize;
use serde::Serialize;

use super::macros::from_row;
use crate::db::utils::ulid::ULID;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    /// ULID string (26 chars, time-sortable)
    pub id: ULID,
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    /// `None` on our own rows. In a group this is the author named by the MLS leaf credential,
    /// not the envelope sender.
    pub sender_ipk: Option<Vec<u8>>,
    pub content: String,
    /// 1 = sent by us, 0 = received
    pub outgoing: bool,
    pub timestamp: u64,
    /// 0 = pending, 1 = sent, 2 = failed, 3 = delivered, 4 = read
    pub status: u8,
    /// Sender-minted 16-byte id, NULL on legacy rows. The cross-device dedup key; the ULID `id`
    /// stays the row key and sort order.
    pub dispatch_id: Option<Vec<u8>>,
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
    /// dispatch_id of the message this one quotes (reply). NULL = plain text.
    pub reply_to: Option<Vec<u8>>,
    /// 0 for an ordinary message, else a `SYSTEM_*` code. On a system row `sender_ipk` is who acted
    /// and `content` names the target: a hex IPK for membership events, the new title for a rename.
    pub system: u8,
}

pub const SYSTEM_NONE: u8 = 0;
pub const SYSTEM_ADDED: u8 = 1;
pub const SYSTEM_LEFT: u8 = 2;
pub const SYSTEM_REMOVED: u8 = 3;
pub const SYSTEM_TITLED: u8 = 4;
/// `sender_ipk` called; `content` is `answered:<seconds>`, `missed`, `declined`, `busy`,
/// `unanswered`, `cancelled` or `failed`. `dispatch_id` is the call id, the same on both phones,
/// so each records the call once.
pub const SYSTEM_CALL: u8 = 5;
/// `sender_ipk` changed a member's role; `content` is `<member hex>:<role>`.
pub const SYSTEM_ROLE: u8 = 6;
/// `sender_ipk` changed a group rule; `content` is `<rule>:<0|1>`, the rule
/// being `add`, `edit`, `send` or `appoint`.
pub const SYSTEM_RULES: u8 = 7;

from_row!(MessageRow { id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to, system });

/// `dispatch_id` names the message and `reactor` the IPK that reacted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    #[serde(with = "serde_bytes")]
    pub dispatch_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub reactor: [u8; 32],
    pub emoji: String,
    pub timestamp: u64,
}

from_row!(ReactionRow { conversation_id, dispatch_id, reactor, emoji, timestamp });

/// A chat of any size. History keys on `id`, minted locally and never changed, because the MLS
/// group behind a chat moves: it is re-created, healed after a restore and adopted on a re-pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRow {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    pub pinned: bool,
    pub muted: bool,
    /// Newest message already alerted for, in unix seconds. Persisted for the wake drain, which
    /// runs in a fresh process.
    pub alerted_at: u64,
    /// 0 = direct (2 members), 1 = group.
    pub kind: u8,
    /// Group name. Empty for a direct chat, which titles itself from the peer.
    pub title: String,
    /// The MLS group behind this chat now; `None` until a first message creates one.
    pub mls_group_id: Option<Vec<u8>>,
    pub created_at: u64,
    /// Who founded the group; `None` for backfilled and direct conversations.
    pub created_by: Option<Vec<u8>>,
}

from_row!(ConversationRow { id, pinned, muted, alerted_at, kind, title, mls_group_id, created_at, created_by });

/// Both parties of a direct chat, us included, get a row, so every roster reads alike.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    #[serde(with = "serde_bytes")]
    pub member_ipk:      [u8; 32],
    /// 0 = member, 1 = admin, 2 = owner.
    pub role:            u8,
    pub joined_at:       u64,
    /// Cleared on leave/remove; the row stays so past messages still attribute.
    pub active: bool,
}

from_row!(MemberRow { conversation_id, member_ipk, role, joined_at, active });

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        "CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            content TEXT NOT NULL,
            outgoing INTEGER NOT NULL,
            timestamp INTEGER NOT NULL,
            status INTEGER NOT NULL DEFAULT 0
        );
    CREATE INDEX idx_messages_peer ON messages(peer_ipk, id DESC);",
    ),
    M::up("ALTER TABLE messages ADD COLUMN dispatch_id BLOB;"),
    // Partial unique index: legacy rows have NULL dispatch_id and must not collide.
    M::up(
        "CREATE UNIQUE INDEX idx_messages_dedup ON messages(peer_ipk, dispatch_id) WHERE dispatch_id IS NOT NULL;",
    ),
    M::up("ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0;"),
    M::up("ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;"),
    M::up(
        "CREATE TABLE reactions (
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            reactor BLOB NOT NULL CHECK(length(reactor) = 32),
            emoji TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            PRIMARY KEY (peer_ipk, dispatch_id, reactor, emoji)
        ) WITHOUT ROWID;
    CREATE INDEX idx_reactions_msg ON reactions(peer_ipk, dispatch_id);",
    ),
    M::up("ALTER TABLE messages ADD COLUMN reply_to BLOB;"),
    // Dispatches already decrypted, whose keys the ratchet has spent. Other homes redeliver them.
    M::up(
        "CREATE TABLE seen_dispatch (
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            seen_at INTEGER NOT NULL,
            PRIMARY KEY (peer_ipk, dispatch_id)
        ) WITHOUT ROWID;",
    ),
    // Per-peer read watermark: the newest incoming dispatch_id the user has read.
    M::up(
        "CREATE TABLE read_state (
            peer_ipk BLOB PRIMARY KEY CHECK(length(peer_ipk) = 32),
            upto_dispatch_id BLOB NOT NULL
        ) WITHOUT ROWID;",
    ),
    // A message's media; its caption stays in `messages.content`.
    M::up(
        "CREATE TABLE message_media (
            peer_ipk    BLOB NOT NULL,
            dispatch_id BLOB NOT NULL,
            kind        INTEGER NOT NULL,
            group_id    BLOB,
            mime        TEXT NOT NULL,
            name        TEXT NOT NULL DEFAULT '',
            size        INTEGER NOT NULL DEFAULT 0,
            width       INTEGER NOT NULL DEFAULT 0,
            height      INTEGER NOT NULL DEFAULT 0,
            blob        BLOB,
            thumb       BLOB,
            file_id     BLOB,
            PRIMARY KEY (peer_ipk, dispatch_id)
        );",
    ),
    // Re-keys chat tables from `peer_ipk` to a local conversation id. Our own member row is added
    // later by the data layer, since our IPK lives in another database.
    M::up(
        r#"
        CREATE TABLE conversations (
            id           BLOB PRIMARY KEY CHECK(length(id) = 16),
            kind         INTEGER NOT NULL DEFAULT 0,
            title        TEXT    NOT NULL DEFAULT '',
            mls_group_id BLOB CHECK(mls_group_id IS NULL OR length(mls_group_id) = 32),
            created_at   INTEGER NOT NULL DEFAULT 0,
            created_by   BLOB CHECK(created_by IS NULL OR length(created_by) = 32)
        ) WITHOUT ROWID;
        CREATE UNIQUE INDEX idx_conversations_group
            ON conversations(mls_group_id) WHERE mls_group_id IS NOT NULL;

        CREATE TABLE conversation_members (
            conversation_id BLOB    NOT NULL CHECK(length(conversation_id) = 16),
            member_ipk      BLOB    NOT NULL CHECK(length(member_ipk) = 32),
            role            INTEGER NOT NULL DEFAULT 0,
            joined_at       INTEGER NOT NULL DEFAULT 0,
            active          INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (conversation_id, member_ipk)
        ) WITHOUT ROWID;
        CREATE INDEX idx_conv_members_ipk ON conversation_members(member_ipk);

        CREATE TEMP TABLE conv_map AS
            SELECT peer_ipk, randomblob(16) AS cid FROM (
                SELECT peer_ipk FROM messages
                UNION SELECT peer_ipk FROM reactions
                UNION SELECT peer_ipk FROM read_state
                UNION SELECT peer_ipk FROM message_media
            );

        INSERT INTO conversations (id, kind, title, mls_group_id, created_at, created_by)
            SELECT cid, 0, '', NULL,
                   COALESCE((SELECT MIN(timestamp) FROM messages WHERE peer_ipk = conv_map.peer_ipk), 0),
                   NULL
            FROM conv_map;
        INSERT INTO conversation_members (conversation_id, member_ipk, role, joined_at, active)
            SELECT cid, peer_ipk, 0, 0, 1 FROM conv_map;

        CREATE TABLE messages_new (
            id              TEXT PRIMARY KEY,
            conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
            sender_ipk      BLOB CHECK(sender_ipk IS NULL OR length(sender_ipk) = 32),
            content         TEXT NOT NULL,
            outgoing        INTEGER NOT NULL,
            timestamp       INTEGER NOT NULL,
            status          INTEGER NOT NULL DEFAULT 0,
            dispatch_id     BLOB,
            edited          INTEGER NOT NULL DEFAULT 0,
            deleted         INTEGER NOT NULL DEFAULT 0,
            reply_to        BLOB
        );
        INSERT INTO messages_new
            (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to)
            SELECT m.id, c.cid,
                   CASE WHEN m.outgoing = 1 THEN NULL ELSE m.peer_ipk END,
                   m.content, m.outgoing, m.timestamp, m.status, m.dispatch_id, m.edited, m.deleted, m.reply_to
            FROM messages m JOIN conv_map c ON c.peer_ipk = m.peer_ipk;
        DROP TABLE messages;
        ALTER TABLE messages_new RENAME TO messages;
        CREATE INDEX idx_messages_conv ON messages(conversation_id, id DESC);
        CREATE UNIQUE INDEX idx_messages_dedup
            ON messages(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL;

        CREATE TABLE reactions_new (
            conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
            dispatch_id     BLOB NOT NULL,
            reactor         BLOB NOT NULL CHECK(length(reactor) = 32),
            emoji           TEXT NOT NULL,
            timestamp       INTEGER NOT NULL,
            PRIMARY KEY (conversation_id, dispatch_id, reactor, emoji)
        ) WITHOUT ROWID;
        INSERT INTO reactions_new
            SELECT c.cid, r.dispatch_id, r.reactor, r.emoji, r.timestamp
            FROM reactions r JOIN conv_map c ON c.peer_ipk = r.peer_ipk;
        DROP TABLE reactions;
        ALTER TABLE reactions_new RENAME TO reactions;
        CREATE INDEX idx_reactions_msg ON reactions(conversation_id, dispatch_id);

        CREATE TABLE read_state_new (
            conversation_id  BLOB PRIMARY KEY CHECK(length(conversation_id) = 16),
            upto_dispatch_id BLOB NOT NULL
        ) WITHOUT ROWID;
        INSERT INTO read_state_new
            SELECT c.cid, r.upto_dispatch_id
            FROM read_state r JOIN conv_map c ON c.peer_ipk = r.peer_ipk;
        DROP TABLE read_state;
        ALTER TABLE read_state_new RENAME TO read_state;

        CREATE TABLE member_read_state (
            conversation_id  BLOB NOT NULL CHECK(length(conversation_id) = 16),
            member_ipk       BLOB NOT NULL CHECK(length(member_ipk) = 32),
            upto_dispatch_id BLOB NOT NULL,
            PRIMARY KEY (conversation_id, member_ipk)
        ) WITHOUT ROWID;

        -- Not conversation-scoped, and deliberately so: this ledger is read
        -- *before* decrypt, where a Welcome or an envelope for a group we
        -- don't hold yet has no conversation to resolve. A dispatch id is
        -- minted by its sender, so (sender, dispatch_id) already identifies a
        -- dispatch uniquely. The old `peer_ipk` was this sender all along.
        CREATE TABLE seen_dispatch_new (
            sender_ipk  BLOB NOT NULL CHECK(length(sender_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            seen_at     INTEGER NOT NULL,
            PRIMARY KEY (sender_ipk, dispatch_id)
        ) WITHOUT ROWID;
        INSERT INTO seen_dispatch_new SELECT peer_ipk, dispatch_id, seen_at FROM seen_dispatch;
        DROP TABLE seen_dispatch;
        ALTER TABLE seen_dispatch_new RENAME TO seen_dispatch;

        CREATE TABLE message_media_new (
            conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
            dispatch_id     BLOB NOT NULL,
            kind            INTEGER NOT NULL,
            group_id        BLOB,
            mime            TEXT NOT NULL,
            name            TEXT NOT NULL DEFAULT '',
            size            INTEGER NOT NULL DEFAULT 0,
            width           INTEGER NOT NULL DEFAULT 0,
            height          INTEGER NOT NULL DEFAULT 0,
            blob            BLOB,
            thumb           BLOB,
            file_id         BLOB,
            PRIMARY KEY (conversation_id, dispatch_id)
        );
        INSERT INTO message_media_new
            SELECT c.cid, m.dispatch_id, m.kind, m.group_id, m.mime, m.name, m.size,
                   m.width, m.height, m.blob, m.thumb, m.file_id
            FROM message_media m JOIN conv_map c ON c.peer_ipk = m.peer_ipk;
        DROP TABLE message_media;
        ALTER TABLE message_media_new RENAME TO message_media;

        DROP TABLE conv_map;
        "#,
    ),
    // System rows are messages, since they order, page and dedup like messages.
    M::up("ALTER TABLE messages ADD COLUMN system INTEGER NOT NULL DEFAULT 0;"),
    // Names peers give themselves in shared groups, keyed by person so one name reads the same in
    // every group. Ranks below `contacts.name`, which the local user chose.
    M::up(
        "CREATE TABLE peer_names ( \
             ipk        BLOB PRIMARY KEY CHECK(length(ipk) = 32), \
             name       TEXT NOT NULL, \
             updated_at INTEGER NOT NULL \
         ) WITHOUT ROWID;",
    ),
    // Pinned, muted and last-alerted live on the conversation, so the backup blob carries them.
    M::up(
        "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0; \
         ALTER TABLE conversations ADD COLUMN muted INTEGER NOT NULL DEFAULT 0; \
         ALTER TABLE conversations ADD COLUMN alerted_at INTEGER NOT NULL DEFAULT 0;",
    ),
    // The user's app-wide settings, kept here so a restore brings them back.
    M::up(
        "CREATE TABLE app_prefs ( \
             key   TEXT PRIMARY KEY, \
             value TEXT NOT NULL \
         ) WITHOUT ROWID;",
    ),
    // Voice note length, which the bubble needs before decoding. 0 for other media.
    M::up("ALTER TABLE message_media ADD COLUMN duration_ms INTEGER NOT NULL DEFAULT 0;"),
    // Persist message references and installed packs; image bytes are cached separately.
    M::up(
        "ALTER TABLE message_media ADD COLUMN sticker BLOB; \
         CREATE TABLE sticker_packs ( \
             pack_id  BLOB PRIMARY KEY CHECK(length(pack_id) = 16), \
             store_id INTEGER NOT NULL, \
             token    BLOB NOT NULL CHECK(length(token) = 32), \
             creator  BLOB NOT NULL CHECK(length(creator) = 32), \
             version  INTEGER NOT NULL, \
             name     TEXT NOT NULL, \
             added_at INTEGER NOT NULL \
         ) WITHOUT ROWID; \
         CREATE TABLE stickers ( \
             pack_id    BLOB NOT NULL, \
             sticker_id BLOB NOT NULL CHECK(length(sticker_id) = 32), \
             position   INTEGER NOT NULL, \
             width      INTEGER NOT NULL, \
             height     INTEGER NOT NULL, \
             PRIMARY KEY (pack_id, sticker_id) \
         ) WITHOUT ROWID; \
         CREATE TABLE sticker_recents ( \
             pack_id    BLOB NOT NULL, \
             sticker_id BLOB NOT NULL, \
             used_at    INTEGER NOT NULL, \
             PRIMARY KEY (pack_id, sticker_id) \
         ) WITHOUT ROWID;",
    ),
    M::up("CREATE TABLE sticker_uploads (
        pack_id BLOB PRIMARY KEY CHECK(length(pack_id) = 16),
        payload BLOB NOT NULL
    ) WITHOUT ROWID;"),
    // Existing and restored history is already seen; new incoming inserts opt in.
    M::up("ALTER TABLE messages ADD COLUMN notification_seen INTEGER NOT NULL DEFAULT 1;
        CREATE INDEX idx_messages_notification_pending ON messages(conversation_id)
            WHERE notification_seen = 0 AND outgoing = 0 AND deleted = 0;"),
    // Pictures peers give themselves, like `peer_names`. A separate table, since a picture must not
    // mint a name row.
    M::up(
        "CREATE TABLE peer_avatars ( \
             ipk        BLOB PRIMARY KEY CHECK(length(ipk) = 32), \
             avif       BLOB NOT NULL, \
             updated_at INTEGER NOT NULL \
         ) WITHOUT ROWID;",
    ),
    // Keep a revision even when the owner removes their picture. A delayed
    // upload must not resurrect it. Preserve pictures from pre-revision builds.
    M::up(
        "ALTER TABLE peer_avatars RENAME TO peer_avatars_unversioned;
         CREATE TABLE peer_avatars (
             ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32),
             avif BLOB,
             updated_at INTEGER NOT NULL,
             revision INTEGER NOT NULL CHECK(revision >= 0)
         ) WITHOUT ROWID;
         INSERT INTO peer_avatars SELECT ipk, avif, updated_at, 0 FROM peer_avatars_unversioned;
         DROP TABLE peer_avatars_unversioned;",
    ),
    // A row also establishes support for avatar reconciliation. Scope by our
    // identity so restoring a different account cannot inherit acknowledgements.
    M::up("CREATE TABLE avatar_acks (
        owner_ipk BLOB NOT NULL CHECK(length(owner_ipk) = 32),
        peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
        revision INTEGER CHECK(revision >= 0),
        PRIMARY KEY(owner_ipk, peer_ipk)
    ) WITHOUT ROWID;"),
    M::up("CREATE TABLE peer_profiles (
        ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32), name TEXT NOT NULL,
        bio TEXT NOT NULL, card BLOB NOT NULL, revision INTEGER NOT NULL CHECK(revision >= 0)
    ) WITHOUT ROWID;
    CREATE TABLE profile_acks (
        owner_ipk BLOB NOT NULL CHECK(length(owner_ipk) = 32),
        peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32), revision INTEGER,
        PRIMARY KEY(owner_ipk, peer_ipk)
    ) WITHOUT ROWID;
    CREATE TABLE group_pictures (
        conversation_id BLOB PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
        revision INTEGER NOT NULL, avif BLOB
    ) WITHOUT ROWID;
    CREATE TABLE contact_requests (
        peer BLOB NOT NULL CHECK(length(peer)=32), outgoing INTEGER NOT NULL,
        name TEXT NOT NULL, card BLOB NOT NULL, expires_ms INTEGER NOT NULL,
        status INTEGER NOT NULL DEFAULT 0, wire BLOB, PRIMARY KEY(peer, outgoing)
    ) WITHOUT ROWID;"),
    // A delete may arrive before its post, even across MLS catch-up. Scoped by author so no member
    // can reserve another's target id; like Seen, it survives clearing history.
    M::up("CREATE TABLE message_deletions (
        conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
        sender_ipk BLOB NOT NULL CHECK(length(sender_ipk) = 32),
        dispatch_id BLOB NOT NULL CHECK(length(dispatch_id) = 16),
        PRIMARY KEY(conversation_id, sender_ipk, dispatch_id)
    ) WITHOUT ROWID;
    INSERT INTO message_deletions
        SELECT conversation_id, sender_ipk, dispatch_id FROM messages
        WHERE outgoing = 0 AND deleted = 1
          AND length(sender_ipk) = 32 AND length(dispatch_id) = 16;"),
    M::up("CREATE TABLE attachment_sharing (
        grant_id BLOB PRIMARY KEY CHECK(length(grant_id) = 32),
        conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
        group_id BLOB NOT NULL CHECK(length(group_id) = 32),
        author BLOB NOT NULL CHECK(length(author) = 32),
        message_id BLOB NOT NULL CHECK(length(message_id) = 16),
        file_id BLOB NOT NULL CHECK(length(file_id) = 32),
        size INTEGER NOT NULL CHECK(size >= 0),
        expires_at INTEGER NOT NULL CHECK(expires_at >= 0),
        recipients BLOB NOT NULL,
        control_id BLOB CHECK(control_id IS NULL OR length(control_id) = 16),
        UNIQUE(conversation_id, author, message_id)
    ) WITHOUT ROWID;
    CREATE INDEX attachment_sharing_file ON attachment_sharing(file_id);
    CREATE TABLE attachment_sharing_revocations (
        conversation_id BLOB NOT NULL CHECK(length(conversation_id) = 16),
        author BLOB NOT NULL CHECK(length(author) = 32),
        message_id BLOB NOT NULL CHECK(length(message_id) = 16),
        PRIMARY KEY(conversation_id, author, message_id)
    ) WITHOUT ROWID;"),
    // No backfill: an older partially-sent attachment has no provable original
    // recipient snapshot. Consuming this intent is part of the first send.
    M::up("CREATE TABLE attachment_sharing_intents (
        message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE
    ) WITHOUT ROWID;"),
    // Receipt history is per original recipient. The old combined member
    // watermark cannot tell delivery from reading and is deliberately not copied.
    M::up("CREATE TABLE message_audiences (
        message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
        complete INTEGER NOT NULL DEFAULT 1
    ) WITHOUT ROWID;
    CREATE TABLE message_recipients (
        message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        member BLOB NOT NULL CHECK(length(member)=32),
        send_status INTEGER NOT NULL DEFAULT 0,
        sent_at INTEGER,
        delivered_at INTEGER,
        read_at INTEGER,
        legacy_status INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(message_id,member)
    ) WITHOUT ROWID;
    CREATE TABLE receipt_peers (
        conversation_id BLOB NOT NULL CHECK(length(conversation_id)=16),
        member BLOB NOT NULL CHECK(length(member)=32),
        PRIMARY KEY(conversation_id,member)
    ) WITHOUT ROWID;
    CREATE TABLE incoming_receipts (
        message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
        delivered_at INTEGER,
        read_at INTEGER,
        is_read INTEGER NOT NULL DEFAULT 0,
        pending INTEGER NOT NULL DEFAULT 0
    ) WITHOUT ROWID;
    INSERT INTO incoming_receipts(message_id,is_read)
        SELECT m.id, CASE WHEN r.upto_dispatch_id IS NOT NULL AND m.dispatch_id<=r.upto_dispatch_id THEN 1 ELSE 0 END
        FROM messages m LEFT JOIN read_state r ON r.conversation_id=m.conversation_id
        WHERE m.outgoing=0 AND m.system=0 AND m.dispatch_id IS NOT NULL;"),
    M::up("DROP TABLE contact_requests;"),
    // A group with signed rules caches its state here. Until it converts, its founder is its one
    // owner.
    M::up("ALTER TABLE conversations ADD COLUMN group_state BLOB;
        UPDATE conversation_members SET role = CASE WHEN member_ipk IS
            (SELECT created_by FROM conversations c WHERE c.id = conversation_id) THEN 2 ELSE 0 END
        WHERE conversation_id IN (SELECT id FROM conversations WHERE kind = 1);"),
    M::up("ALTER TABLE messages ADD COLUMN group_change BLOB;
        CREATE TABLE group_events (
            conversation_id BLOB NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            change_id BLOB NOT NULL,
            PRIMARY KEY(conversation_id, change_id)
        ) WITHOUT ROWID;"),
    M::up("CREATE INDEX idx_media_file ON message_media(file_id) WHERE file_id IS NOT NULL;
        CREATE INDEX idx_messages_dispatch ON messages(dispatch_id) WHERE dispatch_id IS NOT NULL;"),
    // Every incoming message owns a receipt row from here on, so unread and
    // pending counts come off these two indexes instead of a scan.
    M::up("INSERT OR IGNORE INTO incoming_receipts(message_id,is_read)
        SELECT m.id, CASE WHEN r.upto_dispatch_id IS NOT NULL AND m.dispatch_id<=r.upto_dispatch_id THEN 1 ELSE 0 END
        FROM messages m LEFT JOIN read_state r ON r.conversation_id=m.conversation_id
        WHERE m.outgoing=0 AND m.system=0 AND m.dispatch_id IS NOT NULL;
        CREATE INDEX idx_incoming_pending ON incoming_receipts(message_id) WHERE pending=1;
        CREATE INDEX idx_incoming_unread ON incoming_receipts(message_id) WHERE is_read=0;"),
    M::up("CREATE TABLE profile_fields (owner BLOB NOT NULL, field INTEGER NOT NULL,
             object BLOB NOT NULL, version INTEGER NOT NULL, withdrawn INTEGER NOT NULL,
             PRIMARY KEY(owner, field));"),
];
/// A migration's index is its schema version, so the array is append-only: an insert shifts every
/// later version, and a device already past it runs the wrong statements.
pub(super) const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

/// The tables whose commits the client watches.
pub(super) const WATCHED: &[&str] = &[
    "messages",
    "message_recipients",
    "message_audiences",
    "receipt_peers",
    "incoming_receipts",
    "reactions",
    "message_media",
    "conversations",
    "conversation_members",
    "peer_names",
    "peer_avatars",
    "peer_profiles",
    "prefs",
    "group_pictures",
    "sticker_packs",
    "stickers",
    "sticker_recents",
];

pub fn migrate(conn: &mut Connection) {
    super::prepare(conn, &MIGRATIONS);
    // Profile withdrawals and privacy policies must not roll back after power loss.
    conn.pragma_update(None, "synchronous", "FULL").expect("durable message/profile state");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::Message;
    use crate::test_support::data::ulid;

    /// A database a release left at schema `version`, holding what `seed` wrote, upgraded.
    fn migrate_from(version: usize, seed: impl FnOnce(&Connection)) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATION_ARRAY[..version]).to_latest(&mut conn).unwrap();
        seed(&conn);
        migrate(&mut conn);
        conn
    }

    #[test]
    fn receipt_migration_keeps_old_read_state_without_inventing_times_or_audiences() {
        let conn = migrate_from(26, |conn| {
            for n in [1u8, 2] {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, \
                     timestamp, status, dispatch_id) VALUES (?1, ?2, ?3, 'old', 0, 1, 1, ?4)",
                    (ulid(n), [9u8; 16].as_slice(), [2u8; 32].as_slice(), [n; 16].as_slice()),
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO read_state VALUES (?1, ?2)",
                ([9u8; 16].as_slice(), [1u8; 16].as_slice()),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO member_read_state VALUES (?1, ?2, ?3)",
                ([9u8; 16].as_slice(), [2u8; 32].as_slice(), [2u8; 16].as_slice()),
            )
            .unwrap();
        });
        let rows: Vec<(bool, Option<u64>, Option<u64>, bool)> = crate::db::all(
            &conn,
            "SELECT is_read, delivered_at, read_at, pending FROM incoming_receipts ORDER BY message_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
        assert_eq!(rows, vec![(true, None, None, false), (false, None, None, false)]);
        for table in ["message_audiences", "message_recipients"] {
            let n: u32 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "no audience is invented in {table}");
        }
    }

    #[test]
    fn deletion_ledger_migration_preserves_history_and_backfills_only_known_authors() {
        let conn = migrate_from(23, |conn| {
            for (n, outgoing, deleted, sender) in [
                (1u8, false, true, Some([1u8; 32])),
                (2, false, false, Some([1; 32])),
                (3, true, true, None),
                (4, false, true, None),
            ] {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, \
                     timestamp, dispatch_id, deleted) VALUES (?1, ?2, ?3, 'existing', ?4, 123, ?5, ?6)",
                    (ulid(n), [2u8; 16].as_slice(), sender.as_ref().map(|s| s.as_slice()), outgoing,
                     [n; 16].as_slice(), deleted),
                )
                .unwrap();
            }
        });
        let markers: Vec<Vec<u8>> =
            crate::db::all(&conn, "SELECT dispatch_id FROM message_deletions", [], |r| r.get(0))
                .unwrap();
        assert_eq!(markers, vec![vec![1u8; 16]]);
        let kept: u32 = conn
            .query_row("SELECT COUNT(*) FROM messages WHERE content = 'existing'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 4);
    }

    #[test]
    fn legacy_rows_without_dispatch_ids_still_read_back() {
        let peer = [5u8; 32];
        let conn = migrate_from(1, |conn| {
            conn.execute(
                "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status) \
                 VALUES (?1, ?3, 'theirs', 0, 42, 1), (?2, ?3, 'mine', 1, 43, 1)",
                (ulid(1), ulid(2), peer.as_slice()),
            )
            .unwrap();
        });
        let conversation: [u8; 16] =
            conn.query_row("SELECT id FROM conversations", [], |r| r.get(0)).unwrap();
        let rows = Message::get_messages_tx(&conn, &conversation, 10, "").unwrap();
        let me = [7u8; 32];
        let read: Vec<_> = rows
            .iter()
            .map(|r| (r.content.as_str(), r.dispatch_id.clone(), r.sender(&me)))
            .collect();
        assert_eq!(read, vec![("theirs", None, peer), ("mine", None, me)]);
    }
}
