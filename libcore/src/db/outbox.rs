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
}

impl OpType {
    pub fn from_u8(v: u8) -> Option<OpType> {
        match v {
            0 => Some(OpType::Message),
            1 => Some(OpType::Welcome),
            2 => Some(OpType::KpPublish),
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

const MIGRATION_ARRAY: &[M] = &[M::up(
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
)];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static OUTBOX_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("outbox")).expect("db open failed");
    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
