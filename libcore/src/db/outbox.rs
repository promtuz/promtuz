use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::from_row;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum OpType {
    Message = 0,
    Welcome = 1,
    KpPublish = 2,
    /// An MLS control payload (receipt, edit, delete, reaction, pair ack) or a PairDecline, with no
    /// message row behind it.
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
    // Welcome and KpPublish ops may have no target.
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
    // Every member's copy of a send carries the same dispatch id, the message's identity for its
    // receivers, so rows key on (id, target) and a partly acked fan-out can retry the rest.
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
    M::up("CREATE INDEX idx_outbox_due ON outbox(state, next_attempt);"),
    // The epoch a group's latest outboxed commit was built at, written with its copies.
    M::up(
        r#"--sql
        CREATE TABLE commit_left (
          group_id BLOB PRIMARY KEY,
          epoch    INTEGER NOT NULL
        ) WITHOUT ROWID;
    "#,
    ),
];
pub(super) const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub fn migrate(conn: &mut Connection) {
    super::prepare(conn, &MIGRATIONS);
}
