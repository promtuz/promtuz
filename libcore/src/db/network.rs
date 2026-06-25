use anyhow::Result;
use anyhow::bail;
use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;
use serde::Serialize;

use super::macros::PRAGMA;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl TryFrom<String> for CircuitState {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        match s.as_str() {
            "closed" => Ok(Self::Closed),
            "open" => Ok(Self::Open),
            "half_open" => Ok(Self::HalfOpen),
            other => bail!("unknown circuit_state: {}", other),
        }
    }
}

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        r#"--sql
            CREATE TABLE relays (
              id TEXT PRIMARY KEY,
              host TEXT NOT NULL,
              port INTEGER NOT NULL CHECK(port > 0 AND port <= 65535),
              protocol_version INTEGER NOT NULL,
              circuit_state TEXT NOT NULL DEFAULT 'closed' CHECK(circuit_state IN ('closed', 'open', 'half_open')),
              backoff_until INTEGER,
              consecutive_failures INTEGER NOT NULL DEFAULT 0,
              window_attempts INTEGER NOT NULL DEFAULT 0,
              window_successes INTEGER NOT NULL DEFAULT 0,
              window_start INTEGER NOT NULL,
              last_latency INTEGER CHECK(last_latency >= 0),
              last_seen INTEGER NOT NULL,
              last_connect INTEGER,
              last_failure INTEGER
            );
        "#,
    ),
    M::up(
        r#"--sql
            CREATE TABLE relay_latency_samples (
              relay_id TEXT NOT NULL REFERENCES relays(id) ON DELETE CASCADE,
              measured_at INTEGER NOT NULL,
              latency INTEGER NOT NULL CHECK(latency >= 0),
              PRIMARY KEY (relay_id, measured_at)
            );
        "#,
    ),
    M::up("CREATE INDEX idx_relays_circuit_backoff ON relays(circuit_state, backoff_until);"),
    M::up("CREATE INDEX idx_relays_score ON relays(window_successes DESC, last_latency ASC);"),
    M::up(
        "CREATE INDEX idx_latency_samples_relay ON relay_latency_samples(relay_id, measured_at DESC);",
    ),
    // Persist `RelayDescriptor.pubkey` (resolver-vended, authenticated
    // via each relay's signed `RelayHello`). Added for libcore-side
    // TLS-cert SPKI pinning on the Option-A `peer/1` dial path; that
    // path has since been removed (libcore no longer dials peer/1), so
    // this column is now vestigial — retained pending a cleanup pass
    // that drops it along with the resolver wire field.
    //
    // Nullable: rows pre-dating this migration carry NULL.
    M::up(
        r#"--sql
            ALTER TABLE relays ADD COLUMN pubkey BLOB
                CHECK(pubkey IS NULL OR length(pubkey) = 32);
        "#,
    ),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static NETWORK_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("network")).expect("db open failed");
    info!("DB: NETWORK_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
