use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum OpType {
    Message = 0,
    Welcome = 1,
    KpPublish = 2,
    /// MLS control payload (receipt, edit, delete, reaction, pair ack) or a
    /// PairDecline. Carries no message row — retries are pure side-effect.
    Control = 3,
}

impl OpType {
    pub fn from_u8(v: u8) -> Option<OpType> {
        match v {
            0 => Some(OpType::Message),
            1 => Some(OpType::Welcome),
            2 => Some(OpType::KpPublish),
            3 => Some(OpType::Control),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: Vec<u8>,
    pub op_type: u8,
    // Nullable: Welcome/KpPublish ops may carry no target, and rusqlite
    // errors decoding a NULL blob into a non-Option `Vec<u8>`.
    pub target_ipk: Option<Vec<u8>>,
    pub payload: Vec<u8>,
    pub created_at: u64,
    pub attempts: u32,
    pub next_attempt: u64,
}

from_row!(OutboxRow { id, op_type, target_ipk, payload, created_at, attempts, next_attempt });

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        r#"--sql
        CREATE TABLE outbox (
          id           BLOB PRIMARY KEY,
          op_type      INTEGER NOT NULL,
          target_ipk   BLOB,
          payload      BLOB NOT NULL,
          created_at   INTEGER NOT NULL,
          attempts     INTEGER NOT NULL DEFAULT 0,
          next_attempt INTEGER NOT NULL DEFAULT 0,
          state        INTEGER NOT NULL DEFAULT 0   -- 0 pending | 1 dead
        );
    "#,
    ),
    // Per-member delivery rows. One logical send fans out to every member of a
    // conversation, and each copy carries the SAME dispatch id — that id is the
    // message's identity on the receiving side, so replies, edits and reactions
    // across the group all name the same thing. The row key therefore has to be
    // (id, target): keyed on id alone, the second member's copy would collide
    // with the first and a partially-acked fan-out could never retry the rest.
    M::up(
        r#"--sql
        CREATE TABLE outbox_new (
          id           BLOB NOT NULL,
          op_type      INTEGER NOT NULL,
          target_ipk   BLOB,
          payload      BLOB NOT NULL,
          created_at   INTEGER NOT NULL,
          attempts     INTEGER NOT NULL DEFAULT 0,
          next_attempt INTEGER NOT NULL DEFAULT 0,
          state        INTEGER NOT NULL DEFAULT 0
        );
        INSERT INTO outbox_new SELECT id, op_type, target_ipk, payload, created_at, attempts, next_attempt, state FROM outbox;
        DROP TABLE outbox;
        ALTER TABLE outbox_new RENAME TO outbox;
        -- COALESCE, not the bare column: SQLite treats NULLs as distinct in a
        -- unique index, so targetless ops (KeyPackage publishes) would other-
        -- wise duplicate freely instead of deduping on their id.
        CREATE UNIQUE INDEX idx_outbox_key ON outbox(id, COALESCE(target_ipk, X''));
    "#,
    ),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static OUTBOX_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("outbox")).expect("db open failed");
    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
