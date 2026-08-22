use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;
use serde::Deserialize;
use serde::Serialize;

use crate::db::utils::ulid::ULID;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    /// ULID string (26 chars, time-sortable)
    pub id: ULID,
    /// Which conversation this belongs to — the universal chat scope.
    /// Locally minted and immutable; see [`ConversationRow::mls_group_id`]
    /// for why the MLS group id can't hold this job.
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    /// Who spoke, when that isn't us. `None` = the local user, which is how
    /// every outgoing row and every pre-group inbound row reads. In a group
    /// this is the authoritative inner MLS leaf credential, not the outer
    /// envelope sender.
    pub sender_ipk: Option<Vec<u8>>,
    pub content: String,
    /// 1 = sent by us, 0 = received
    pub outgoing: bool,
    pub timestamp: u64,
    /// 0 = pending, 1 = sent, 2 = failed
    pub status: u8,
    /// Sender-minted monotonic id (16 bytes); NULL on legacy rows.
    /// Cross-device dedup + convergence key — the ULID `id` stays the
    /// row PK / ordering key.
    pub dispatch_id: Option<Vec<u8>>,
    /// Sender edited this message's text after sending.
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
    /// dispatch_id of the message this one quotes (reply). NULL = plain text.
    pub reply_to: Option<Vec<u8>>,
    /// 0 for an ordinary message, else a `SYSTEM_*` code narrating a
    /// membership or title change. On a system row `sender_ipk` is who acted
    /// and `content` names the target — a hex IPK for the membership events,
    /// the new title for a rename.
    pub system: u8,
}

/// Not a system row — an ordinary message.
pub const SYSTEM_NONE: u8 = 0;
pub const SYSTEM_ADDED: u8 = 1;
pub const SYSTEM_LEFT: u8 = 2;
pub const SYSTEM_REMOVED: u8 = 3;
pub const SYSTEM_TITLED: u8 = 4;

from_row!(MessageRow { id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to, system });

/// One emoji reaction on a message. Keyed by `reactor` (an IPK, not a
/// me/them bool) so a multi-member group attributes each reaction to its
/// author. `dispatch_id` names the reacted message.
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

/// A chat, of any size. The scope every message, reaction, receipt and media
/// row hangs off.
///
/// `id` is minted locally and never changes. `mls_group_id` is a *pointer* to
/// the crypto group currently backing this conversation, and it moves: the
/// send path re-creates a group whose local state went missing, `heal_dead_group`
/// re-establishes after a restore, and an inbound Welcome adopts the peer's
/// group on a re-pair. Keying history on the group id would strand it on every
/// one of those; keying on `id` and repointing this column survives them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRow {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    /// Kept at the top of the home list.
    #[serde(default)]
    pub pinned: bool,
    /// No notifications from this chat.
    #[serde(default)]
    pub muted: bool,
    /// Newest message we already alerted for, in unix seconds. Persisted rather
    /// than held in memory: the case that needs it is a wake-drain in a fresh
    /// process, whose heap is empty and whose unread set is hours old.
    #[serde(default)]
    pub alerted_at: u64,
    /// 0 = direct (2 members), 1 = group.
    pub kind: u8,
    /// Group name. Empty for a direct chat, which titles itself from the peer.
    pub title: String,
    /// The MLS group currently backing this conversation, or `None` before one
    /// has been created (a contact added but never messaged).
    pub mls_group_id: Option<Vec<u8>>,
    pub created_at: u64,
    /// Who founded the group; `None` for backfilled and direct conversations.
    pub created_by: Option<Vec<u8>>,
}

from_row!(ConversationRow { id, pinned, muted, alerted_at, kind, title, mls_group_id, created_at, created_by });

/// One member's place in a conversation. Both parties of a direct chat get a
/// row, including us, so the roster reads the same for 1:1 and N.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    #[serde(with = "serde_bytes")]
    pub member_ipk: [u8; 32],
    /// 0 = member, 1 = admin. v1: the creator is the sole admin.
    pub role: u8,
    pub joined_at: u64,
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
    // Delivery dedup ledger: a dispatch we already decrypted must never be
    // re-decrypted (the MLS ratchet consumed its key → SecretReuseError).
    // Redelivery from the other K-home relays on reconnect is the trigger.
    M::up(
        "CREATE TABLE seen_dispatch (
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            seen_at INTEGER NOT NULL,
            PRIMARY KEY (peer_ipk, dispatch_id)
        ) WITHOUT ROWID;",
    ),
    // Local read high-water-mark per peer: the newest incoming dispatch_id the
    // user has read. Drives the home-list unread count; mark_read upserts it.
    M::up(
        "CREATE TABLE read_state (
            peer_ipk BLOB PRIMARY KEY CHECK(length(peer_ipk) = 32),
            upto_dispatch_id BLOB NOT NULL
        ) WITHOUT ROWID;",
    ),
    // Per-message media metadata (Image inline bytes / Attachment thumb +
    // file_id), keyed to the message it belongs to. The caption stays on
    // messages.content; this only holds the media side of the payload.
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
    // The conversation re-key. Every chat-scoped table moves off `peer_ipk`
    // and onto a locally-minted conversation id, and `messages` gains the
    // in-group `sender_ipk` the scope column used to double as.
    //
    // One migration, five table rebuilds: SQLite can't retype a column in
    // place, and doing them separately would rewrite `messages` twice.
    //
    // Existing rows are carried across on a peer→conversation map built from
    // every table that referenced a peer, so a chat that only ever had, say, a
    // read watermark still gets its conversation. Our own membership row is
    // added lazily by the data layer — the local IPK lives in another database
    // and static migration SQL can't reach it.
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
    // System rows: membership and title changes narrated inline with the
    // messages they sit between. A column rather than a side table because
    // they order, page and dedup exactly like messages do — the only thing
    // that differs is how they render.
    M::up("ALTER TABLE messages ADD COLUMN system INTEGER NOT NULL DEFAULT 0;"),
    // What a peer calls themselves, as told to a group we share. Keyed on the
    // person rather than on the conversation: the same someone in two groups is
    // one someone, and a name learned in either should read the same in both.
    //
    // Never a substitute for `contacts.name` — that one the local user chose,
    // this one its subject asserted. Resolution keeps them in that order.
    M::up(
        "CREATE TABLE peer_names ( \
             ipk        BLOB PRIMARY KEY CHECK(length(ipk) = 32), \
             name       TEXT NOT NULL, \
             updated_at INTEGER NOT NULL \
         ) WITHOUT ROWID;",
    ),
    // Pinned / muted / last-alerted were SharedPreferences, which Auto Backup
    // does not carry — `backup_rules.xml` ships the blob and nothing else — so
    // they were quietly lost on every reinstall. They are facts about a
    // conversation, so they live on it and ride the blob with it.
    M::up(
        "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0; \
         ALTER TABLE conversations ADD COLUMN muted INTEGER NOT NULL DEFAULT 0; \
         ALTER TABLE conversations ADD COLUMN alerted_at INTEGER NOT NULL DEFAULT 0;",
    ),
    // App-wide settings that are the user's, not the device's, so a restore
    // brings them back. Stringly-typed on purpose: a settings row is read once
    // by a screen that already knows what it means.
    M::up(
        "CREATE TABLE app_prefs ( \
             key   TEXT PRIMARY KEY, \
             value TEXT NOT NULL \
         ) WITHOUT ROWID;",
    ),
];
/// A migration's index in the array *is* its schema version, so the array is
/// append-only: inserting one shifts every later version, and a device already
/// past that point re-runs the wrong statements. Add at the end, always.
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static MESSAGES_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("messages")).expect("db open failed");
    PRAGMA!(conn, MIGRATIONS);
    super::register_change_hook(&conn, &[
        "messages",
        "reactions",
        "message_media",
        "conversations",
        "conversation_members",
        "peer_names",
    ]);

    Mutex::new(conn)
});

#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    PRAGMA!(conn, MIGRATIONS);
    conn
}
