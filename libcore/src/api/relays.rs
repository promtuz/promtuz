//! Relay diagnostics exports: read the stored relay set + health/latency,
//! and the three dev actions (reset circuit, forget, reconnect).

use std::collections::HashMap;

use rusqlite::params;

use crate::db::network::CircuitState;
use crate::db::network::NETWORK_DB;
use crate::platform::CoreError;

/// Circuit-breaker state, projected for the client.
#[derive(uniffi::Enum)]
pub enum RelayCircuit {
    Closed,
    Open,
    HalfOpen,
}

impl From<CircuitState> for RelayCircuit {
    fn from(s: CircuitState) -> Self {
        match s {
            CircuitState::Closed => RelayCircuit::Closed,
            CircuitState::Open => RelayCircuit::Open,
            CircuitState::HalfOpen => RelayCircuit::HalfOpen,
        }
    }
}

/// A stored relay with its health/latency, projected for the client.
#[derive(uniffi::Record)]
pub struct RelayStat {
    pub id:                   String,
    pub host:                 String,
    pub port:                 u16,
    pub circuit_state:        RelayCircuit,
    pub consecutive_failures: u32,
    pub window_attempts:      u32,
    pub window_successes:     u32,
    /// Latest measured RTT in ms, `None` if never connected.
    pub last_latency:         Option<u64>,
    /// Last successful connect (ms epoch), `None` if never connected.
    pub last_connect:         Option<u64>,
    /// When the open circuit next admits a probe (ms epoch), if backing off.
    pub backoff_until:        Option<u64>,
    /// True for the one relay currently serving as the live home connection.
    pub is_connected:         bool,
    /// RTT history (ms), oldest→newest, for the latency graph.
    pub latency_samples:      Vec<u64>,
}

/// All stored relays with health + latency history. Read-only snapshot;
/// the client polls this for a live view (there is no relay event stream).
#[uniffi::export]
pub fn get_relays() -> Result<Vec<RelayStat>, CoreError> {
    let conn = NETWORK_DB.lock();
    let connected_id = crate::state::RELAY.read().as_ref().map(|r| r.id.to_string());

    // One grouped read of the sample buffer, keyed by relay.
    let mut samples: HashMap<String, Vec<u64>> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT relay_id, latency FROM relay_latency_samples ORDER BY measured_at ASC")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(db_err)?;
        for row in rows {
            let (id, latency) = row.map_err(db_err)?;
            samples.entry(id).or_default().push(latency);
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT id, host, port, circuit_state, consecutive_failures,
                    window_attempts, window_successes, last_latency,
                    last_connect, backoff_until
             FROM relays
             ORDER BY last_connect DESC",
        )
        .map_err(db_err)?;

    let relays = stmt
        .query_map([], |row| {
            let id: String = row.get("id")?;
            let circuit: String = row.get("circuit_state")?;
            let circuit = CircuitState::try_from(circuit)
                .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
            Ok(RelayStat {
                latency_samples:      samples.remove(&id).unwrap_or_default(),
                is_connected:         connected_id.as_deref() == Some(id.as_str()),
                circuit_state:        circuit.into(),
                host:                 row.get("host")?,
                port:                 row.get::<_, i64>("port")? as u16,
                consecutive_failures: row.get::<_, i64>("consecutive_failures")? as u32,
                window_attempts:      row.get::<_, i64>("window_attempts")? as u32,
                window_successes:     row.get::<_, i64>("window_successes")? as u32,
                last_latency:         row.get::<_, Option<i64>>("last_latency")?.map(|v| v as u64),
                last_connect:         row.get::<_, Option<i64>>("last_connect")?.map(|v| v as u64),
                backoff_until:        row.get::<_, Option<i64>>("backoff_until")?.map(|v| v as u64),
                id,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;

    Ok(relays)
}

/// Un-trip a relay's circuit breaker so relay selection reconsiders it now.
#[uniffi::export]
pub fn reset_relay_circuit(id: String) -> Result<(), CoreError> {
    let conn = NETWORK_DB.lock();
    conn.execute(
        "UPDATE relays SET circuit_state = 'closed', backoff_until = NULL, consecutive_failures = 0
         WHERE id = ?1",
        params![id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Delete a relay + its latency samples. The resolver re-adds it on the
/// next fetch, so this is a local reset, not a permanent block.
#[uniffi::export]
pub fn forget_relay(id: String) -> Result<(), CoreError> {
    let conn = NETWORK_DB.lock();
    // relay_latency_samples cascade via the FK (foreign_keys = ON).
    conn.execute("DELETE FROM relays WHERE id = ?1", params![id]).map_err(db_err)?;
    Ok(())
}

/// Connect (or reconnect) to a specific relay by id. Queues it as the relay
/// loop's next pick and drops the current connection so the switch happens
/// promptly; if nothing is connected, the loop picks it up on its next cycle.
#[uniffi::export]
pub fn connect_relay(id: String) -> Result<(), CoreError> {
    crate::state::set_preferred_relay(id);
    if let Some(relay) = crate::state::RELAY.read().as_ref()
        && let Some(conn) = &relay.connection
    {
        conn.close(quinn::VarInt::from_u32(0), b"user switch relay");
    }
    Ok(())
}

fn db_err(e: rusqlite::Error) -> CoreError {
    CoreError::Internal { msg: e.to_string() }
}
