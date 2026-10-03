use std::sync::Arc;

use anyhow::Result;
use common::PROTOCOL_VERSION;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::RelayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::UnpackError;
use common::proto::pack::Unpacker;
use common::utils::now_ms;
use log::info;
use rusqlite::Connection;
use rusqlite::params;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::data::ResolverSeed;
use crate::db::all;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::quic::dialer::DialerError;
use crate::quic::dialer::connect_to_any_seed;
use crate::quic::dialer::quinn_err;
use crate::state::core;

const FAILURE_THRESHOLD: u32 = 3;
const BACKOFF_BASE_MS: u64 = 5_000;
const BACKOFF_MAX_MS: u64 = 30 * 60 * 1_000;
/// Shorter than [`BACKOFF_MAX_MS`]: the fault may be on the path rather than the relay's cert, and
/// a re-resolve to a new address clears it.
const BACKOFF_TERMINAL_MS: u64 = 2 * 60 * 1_000;
const WINDOW_DURATION_MS: u64 = 10 * 60 * 1_000;
const LATENCY_SAMPLE_LIMIT: i64 = 50;
const SCORE_WEIGHT_SUCCESS: f64 = 0.6;
const SCORE_WEIGHT_LATENCY: f64 = 0.4;
const EXPLORE_PROBABILITY: f64 = 0.2;
const TOP_N: usize = 3;

/// The `CASE` arms read the pre-update values, so a relay whose address changed starts from a clean
/// circuit: its failures were recorded against the old address.
const REFRESH_UPSERT: &str = "\
    INSERT INTO relays (id, host, port, last_seen, protocol_version, window_start, pubkey)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
    ON CONFLICT(id) DO UPDATE SET
      circuit_state        = CASE WHEN host <> excluded.host OR port <> excluded.port
                                  THEN 'closed' ELSE circuit_state END,
      backoff_until        = CASE WHEN host <> excluded.host OR port <> excluded.port
                                  THEN NULL ELSE backoff_until END,
      consecutive_failures = CASE WHEN host <> excluded.host OR port <> excluded.port
                                  THEN 0 ELSE consecutive_failures END,
      host                 = excluded.host,
      port                 = excluded.port,
      last_seen            = excluded.last_seen,
      protocol_version     = excluded.protocol_version,
      pubkey               = excluded.pubkey";

/// A relay row. The live connection to it is a [`crate::quic::server::Session`].
#[derive(Clone)]
pub struct Relay {
    pub id:     Arc<str>,
    pub host:   Arc<str>,
    pub port:   u16,
    /// Unread: nothing pins a relay cert. TODO: drop it with the DB column and the resolver field.
    pub pubkey: Option<[u8; 32]>,
    /// Kept on the row, so a P2P session can pick an assist bridge it is not connected to.
    pub assist: bool,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("pubkey", &self.pubkey.as_ref().map(|pk| hex::encode(&pk[..4])))
            .finish()
    }
}

#[derive(Error, Debug)]
pub enum RelayError {
    #[error("no relay available matching criteria")]
    NoneAvailable,

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
}

#[derive(Error, Debug)]
pub enum ResolveError {
    #[error("resolver did not return any relay")]
    EmptyResponse,

    #[error("dialer error: {0}")]
    DialerError(#[from] DialerError),

    #[error("failed to unpack: {0}")]
    UnpackError(#[from] UnpackError),

    #[error("relay error: {0}")]
    RelayError(#[from] RelayError),
}

impl Relay {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id:     row.get("id")?,
            host:   row.get("host")?,
            port:   row.get("port")?,
            pubkey: row.get("pubkey")?,
            assist: row.get("assist")?,
        })
    }

    pub fn fetch_best() -> Result<Self, RelayError> {
        Self::fetch_best_tx(&core().db.network().lock())
    }

    pub(crate) fn fetch_best_tx(conn: &Connection) -> Result<Self, RelayError> {
        let now = now_ms() as i64;

        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "UPDATE relays SET circuit_state = 'half_open'
             WHERE circuit_state = 'open'
               AND backoff_until IS NOT NULL
               AND backoff_until <= ?1",
            params![now],
        )?;

        struct Candidate {
            relay:        Relay,
            latency:      Option<i64>,
            success_rate: f64,
        }

        let rows: Vec<Candidate> = all(
            &tx,
            "SELECT *, CAST(window_successes AS REAL) / MAX(window_attempts, 1) AS success_rate
             FROM relays
             WHERE protocol_version = ?1
               AND circuit_state IN ('closed', 'half_open')",
            params![PROTOCOL_VERSION],
            |row| {
                Ok(Candidate {
                    relay:        Self::from_row(row)?,
                    latency:      row.get("last_latency")?,
                    success_rate: row.get("success_rate")?,
                })
            },
        )?;

        tx.commit()?;

        if rows.is_empty() {
            return Err(RelayError::NoneAvailable);
        }

        let mut scored: Vec<(f64, &Candidate)> = {
            let min_lat = rows.iter().filter_map(|c| c.latency).min().unwrap_or(0);
            let max_lat = rows.iter().filter_map(|c| c.latency).max().unwrap_or(0);

            rows.iter()
                .map(|c| {
                    let norm_latency = match (c.latency, max_lat > min_lat) {
                        (Some(l), true) => (l - min_lat) as f64 / (max_lat - min_lat) as f64,
                        (Some(_), false) => 0.0,
                        (None, _) => 1.0,
                    };
                    let score = SCORE_WEIGHT_SUCCESS * c.success_rate
                        + SCORE_WEIGHT_LATENCY * (1.0 - norm_latency);
                    (score, c)
                })
                .collect()
        };

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let chosen = if scored.len() > TOP_N && rand::random::<f64>() < EXPLORE_PROBABILITY {
            let tail = &scored[TOP_N..];
            let idx = (rand::random::<f64>() * tail.len() as f64) as usize;
            tail[idx.min(tail.len() - 1)].1
        } else {
            let pool: &[(f64, &Candidate)] = &scored[..TOP_N.min(scored.len())];
            let total: f64 = pool.iter().map(|(s, _)| s).sum();
            let mut pick = rand::random::<f64>() * total;
            let mut chosen = pool.last().unwrap().1;
            for (score, candidate) in pool {
                pick -= score;
                if pick <= 0.0 {
                    chosen = candidate;
                    break;
                }
            }
            chosen
        };

        Ok(chosen.relay.clone())
    }

    /// Bypasses scoring: a bridge that answers beats a faster one that drops the datagrams.
    pub fn fetch_assist_capable() -> Option<Self> {
        Self::fetch_assist_capable_tx(&core().db.network().lock())
    }

    pub(crate) fn fetch_assist_capable_tx(conn: &Connection) -> Option<Self> {
        conn.query_row(
            "SELECT * FROM relays
              WHERE assist = 1 AND circuit_state IN ('closed', 'half_open')
              ORDER BY last_latency IS NULL, last_latency ASC
              LIMIT 1",
            [],
            Self::from_row,
        )
        .ok()
    }

    pub fn record_assist(&self, assist: bool) -> Result<(), RelayError> {
        self.record_assist_tx(&core().db.network().lock(), assist)
    }

    pub(crate) fn record_assist_tx(&self, conn: &Connection, assist: bool) -> Result<(), RelayError> {
        conn.execute(
            "UPDATE relays SET assist = ?1 WHERE id = ?2",
            params![assist as i64, self.id.as_ref()],
        )?;
        Ok(())
    }

    /// The relay whose backoff ends first, so an all-open table is still probed.
    pub fn fetch_backoff_candidate() -> Result<Self, RelayError> {
        Self::fetch_backoff_candidate_tx(&core().db.network().lock())
    }

    pub(crate) fn fetch_backoff_candidate_tx(conn: &Connection) -> Result<Self, RelayError> {
        let id: String = conn
            .query_row(
                "SELECT id FROM relays WHERE protocol_version = ?1 ORDER BY backoff_until LIMIT 1",
                params![PROTOCOL_VERSION],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => RelayError::NoneAvailable,
                other => RelayError::Db(other),
            })?;
        Self::fetch_by_id_tx(conn, &id)
    }

    /// A new network invalidates every open circuit.
    pub fn reset_circuits() -> Result<(), RelayError> {
        Self::reset_circuits_tx(&core().db.network().lock())
    }

    pub(crate) fn reset_circuits_tx(conn: &Connection) -> Result<(), RelayError> {
        conn.execute(
            "UPDATE relays SET circuit_state = 'closed', backoff_until = NULL, consecutive_failures = 0
             WHERE circuit_state != 'closed'",
            [],
        )?;
        Ok(())
    }

    pub fn fetch_by_id(id: &str) -> Result<Self, RelayError> {
        Self::fetch_by_id_tx(&core().db.network().lock(), id)
    }

    pub(crate) fn fetch_by_id_tx(conn: &Connection, id: &str) -> Result<Self, RelayError> {
        conn.query_row("SELECT * FROM relays WHERE id = ?1", params![id], Self::from_row).map_err(
            |e| match e {
                rusqlite::Error::QueryReturnedNoRows => RelayError::NoneAvailable,
                other => RelayError::Db(other),
            },
        )
    }

    pub fn refresh(relays: &[RelayDescriptor]) -> Result<(), RelayError> {
        Self::refresh_tx(&core().db.network().lock(), relays)
    }

    pub(crate) fn refresh_tx(conn: &Connection, relays: &[RelayDescriptor]) -> Result<(), RelayError> {
        let now = now_ms();

        let mut stmt = conn.prepare(REFRESH_UPSERT)?;

        for r in relays {
            stmt.execute(params![
                r.id.to_string(),
                r.addr.ip().to_string(),
                r.addr.port(),
                now,
                PROTOCOL_VERSION,
                now,
                r.pubkey.0.as_slice(),
            ])?;
        }

        Ok(())
    }

    /// Latency comes from live RTT via [`Relay::record_rtt`], not the handshake time.
    pub fn record_success(&self) -> Result<(), RelayError> {
        self.record_success_tx(&core().db.network().lock())
    }

    pub(crate) fn record_success_tx(&self, conn: &Connection) -> Result<(), RelayError> {
        let now = now_ms() as i64;
        let window_threshold = now - WINDOW_DURATION_MS as i64;

        conn.execute(
            "UPDATE relays SET
                   circuit_state        = 'closed',
                   backoff_until        = NULL,
                   consecutive_failures = 0,
                   last_connect         = ?1,
                   window_attempts      = CASE WHEN window_start < ?2 THEN 1 ELSE window_attempts + 1 END,
                   window_successes     = CASE WHEN window_start < ?2 THEN 1 ELSE window_successes + 1 END,
                   window_start         = CASE WHEN window_start < ?2 THEN ?1 ELSE window_start END
                 WHERE id = ?3",
            params![now, window_threshold, self.id.as_ref()],
        )?;

        Ok(())
    }

    pub fn record_rtt(&self, rtt_ms: u64) -> Result<(), RelayError> {
        self.record_rtt_tx(&core().db.network().lock(), rtt_ms)
    }

    pub(crate) fn record_rtt_tx(&self, conn: &Connection, rtt_ms: u64) -> Result<(), RelayError> {
        let now = now_ms() as i64;

        conn.execute(
            "UPDATE relays SET last_latency = ?1 WHERE id = ?2",
            params![rtt_ms as i64, self.id.as_ref()],
        )?;

        // A second sample in the same millisecond collides on the key and is harmlessly dropped.
        conn.execute(
            "INSERT OR IGNORE INTO relay_latency_samples (relay_id, measured_at, latency)
             VALUES (?1, ?2, ?3)",
            params![self.id.as_ref(), now, rtt_ms as i64],
        )?;

        conn.execute(
            "DELETE FROM relay_latency_samples
            WHERE relay_id = ?1
            AND rowid NOT IN (
              SELECT rowid FROM relay_latency_samples
              WHERE relay_id = ?1
              ORDER BY measured_at DESC
              LIMIT ?2
            )",
            params![self.id.as_ref(), LATENCY_SAMPLE_LIMIT],
        )?;

        Ok(())
    }

    /// Cert and auth failures do not clear within a retry loop, so the circuit opens at once.
    pub fn record_terminal_failure(&self) -> Result<(), RelayError> {
        self.record_terminal_failure_tx(&core().db.network().lock())
    }

    pub(crate) fn record_terminal_failure_tx(&self, conn: &Connection) -> Result<(), RelayError> {
        let now = now_ms() as i64;
        let backoff = BACKOFF_TERMINAL_MS as i64;

        conn.execute(
            "UPDATE relays SET
                   circuit_state        = 'open',
                   backoff_until        = ?1,
                   consecutive_failures = consecutive_failures + 1,
                   last_failure         = ?2
                 WHERE id = ?3",
            params![now + backoff, now, self.id.as_ref()],
        )?;

        info!(
            "relay({}) terminal failure (cert/auth) — circuit open until {}",
            self.id,
            now + backoff
        );

        Ok(())
    }

    pub fn record_failure(&self) -> Result<(), RelayError> {
        self.record_failure_tx(&core().db.network().lock())
    }

    pub(crate) fn record_failure_tx(&self, conn: &Connection) -> Result<(), RelayError> {
        let now = now_ms() as i64;
        let window_threshold = now - WINDOW_DURATION_MS as i64;

        let consecutive_failures: u32 = conn.query_row(
            "UPDATE relays SET
                   consecutive_failures = consecutive_failures + 1,
                   last_failure         = ?1,
                   window_attempts      = CASE WHEN window_start < ?3 THEN 1 ELSE window_attempts + 1 END,
                   window_start         = CASE WHEN window_start < ?3 THEN ?1 ELSE window_start END
                 WHERE id = ?2
                 RETURNING consecutive_failures",
            params![now, self.id.as_ref(), window_threshold],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )?;

        if consecutive_failures >= FAILURE_THRESHOLD {
            let exp = (consecutive_failures - FAILURE_THRESHOLD).min(10);
            let backoff = (BACKOFF_BASE_MS * (1u64 << exp)).min(BACKOFF_MAX_MS) as i64;

            info!("relay({}) opening circuit, backoff {}ms", self.id, backoff);

            conn.execute(
                "UPDATE relays SET
                       circuit_state = 'open',
                       backoff_until = ?1
                     WHERE id = ?2",
                params![(now + backoff), self.id.as_ref()],
            )?;
        }

        Ok(())
    }
}

impl Relay {
    pub async fn resolve(seeds: &[ResolverSeed]) -> Result<(), ResolveError> {
        use ConnectionState as CS;

        CS::Resolving.emit();

        let conn = connect_to_any_seed(seeds).await.inspect_err(|_| CS::Failed.emit())?;

        let req = ClientRequest::GetRelays().pack().unwrap();

        let (mut send, mut recv) = conn.open_bi().await.map_err(quinn_err)?;
        send.write_all(&req).await.map_err(quinn_err)?;
        send.flush().await.map_err(quinn_err)?;

        loop {
            let client_resp = ClientResponse::unpack(&mut recv).await?;

            #[allow(irrefutable_let_patterns)]
            if let ClientResponse::GetRelays { relays } = client_resp {
                if relays.is_empty() {
                    break Err(ResolveError::EmptyResponse);
                }

                info!("resolver returned {} relay(s)", relays.len());
                Relay::refresh(&relays)?;
                conn.close(quinn::VarInt::from_u32(1), &[]);

                break Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use common::types::bytes::Bytes;
    use common::types::id::NodeId;

    use super::*;
    use crate::test_support::data::open;
    use crate::test_support::data::with_failing_trigger;

    fn descriptor(addr: &str) -> RelayDescriptor {
        RelayDescriptor {
            id:     NodeId::from_bytes([1; 32]),
            addr:   addr.parse().unwrap(),
            pubkey: Bytes([2; 32]),
        }
    }

    /// State, backoff length in ms, consecutive failures.
    fn circuit(conn: &Connection) -> (String, Option<i64>, u32) {
        conn.query_row(
            "SELECT circuit_state, backoff_until - last_failure, consecutive_failures FROM relays",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn failures_open_the_circuit_with_backoff_and_a_success_closes_it() {
        let conn = open(crate::db::network::migrate);
        Relay::refresh_tx(&conn, &[descriptor("10.0.0.1:443")]).unwrap();
        let relay = Relay::fetch_best_tx(&conn).unwrap();
        for n in 1..FAILURE_THRESHOLD {
            relay.record_failure_tx(&conn).unwrap();
            assert_eq!(circuit(&conn), ("closed".into(), None, n));
        }
        relay.record_failure_tx(&conn).unwrap();
        let base = BACKOFF_BASE_MS as i64;
        assert_eq!(circuit(&conn), ("open".into(), Some(base), FAILURE_THRESHOLD));
        relay.record_failure_tx(&conn).unwrap();
        assert_eq!(circuit(&conn), ("open".into(), Some(2 * base), FAILURE_THRESHOLD + 1));
        assert!(matches!(Relay::fetch_best_tx(&conn), Err(RelayError::NoneAvailable)));
        assert_eq!(
            Relay::fetch_backoff_candidate_tx(&conn).unwrap().id,
            relay.id,
            "a lone relay is still probed"
        );

        Relay::refresh_tx(&conn, &[descriptor("10.0.0.1:443")]).unwrap();
        assert_eq!(circuit(&conn).0, "open", "a refresh at the same address keeps the circuit");
        Relay::refresh_tx(&conn, &[descriptor("10.0.0.1:8443")]).unwrap();
        assert_eq!(circuit(&conn), ("closed".into(), None, 0), "a relay that moved starts clean");

        relay.record_terminal_failure_tx(&conn).unwrap();
        assert_eq!(circuit(&conn), ("open".into(), Some(BACKOFF_TERMINAL_MS as i64), 1));
        conn.execute("UPDATE relays SET backoff_until = 1", []).unwrap();
        assert_eq!(
            Relay::fetch_best_tx(&conn).unwrap().id,
            relay.id,
            "an expired backoff is retried"
        );
        assert_eq!(circuit(&conn).0, "half_open");
        relay.record_success_tx(&conn).unwrap();
        assert_eq!(circuit(&conn), ("closed".into(), None, 0));
    }

    #[test]
    fn an_error_inside_fetch_best_leaves_no_transaction_open() {
        let mut conn = open(crate::db::network::migrate);
        Relay::refresh_tx(&conn, &[descriptor("10.0.0.1:443")]).unwrap();
        conn.execute("UPDATE relays SET circuit_state = 'open', backoff_until = 1", []).unwrap();
        with_failing_trigger(&mut conn, "relays", "UPDATE", |conn| {
            assert!(Relay::fetch_best_tx(conn).is_err());
            assert!(conn.is_autocommit(), "the failed pass rolled back");
        });
        assert!(Relay::fetch_best_tx(&conn).is_ok(), "the next pass runs as usual");
    }
}
