use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::RelayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::UnpackError;
use common::proto::pack::Unpacker;
use log::info;
use quinn::Connection;
use rusqlite::params;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::data::ResolverSeed;
use crate::db::network::CircuitState;
use crate::db::network::NETWORK_DB;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::quic::dialer::DialerError;
use crate::quic::dialer::connect_to_any_seed;
use crate::quic::dialer::quinn_err;
use crate::utils::systime;

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===:  CONST  :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

const FAILURE_THRESHOLD: u32 = 3;
const BACKOFF_BASE_MS: u64 = 5_000;
const BACKOFF_MAX_MS: u64 = 30 * 60 * 1_000;
const WINDOW_DURATION_MS: u64 = 10 * 60 * 1_000;
const LATENCY_SAMPLE_LIMIT: i64 = 50;
const SCORE_WEIGHT_SUCCESS: f64 = 0.6;
const SCORE_WEIGHT_LATENCY: f64 = 0.4;
const EXPLORE_PROBABILITY: f64 = 0.2;
const TOP_N: usize = 3;

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: STRUCTS :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

/// Shareable statistical data
#[derive(Debug, Serialize)]
pub struct RelayInfo {
    pub id:                   String,
    pub host:                 String,
    pub port:                 u16,
    pub circuit_state:        CircuitState,
    pub consecutive_failures: u32,
    pub window_attempts:      u32,
    pub window_successes:     u32,
    pub last_latency:         Option<u64>,
    pub last_seen:            u64,
    pub last_connect:         Option<u64>,
}

/// Relay instance
#[derive(Debug, Clone)]
pub struct Relay {
    pub id:         Arc<str>,
    pub host:       Arc<str>,
    pub port:       u16,
    /// Contains quinn connection IF connected
    pub connection: Option<Connection>,
}

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===:  ERROR  :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

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

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===:  IMPLE  :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

/// # Working Model
///
/// ## Score model
///
/// Instead of a linear count, each relay should have a composite score built from weighted factors.
/// Something like:
///
/// - Latency (measured, not assumed) - lower is better, normalized against the known range
/// - Success rate - successes / total attempts over a rolling window, not a lifetime counter
/// - Consecutive failures - separate from success rate, used for circuit breaking
/// - Last seen - how recently did it work at all
///
/// Weight them however the use case demands. If it's latency-sensitive, weight that heavily. If
/// reliability matters more, weight success rate.
///
/// ## Rolling window, not lifetime
///
/// A relay that was bad 3 months ago and great for the last week should rank well.
/// Use a time-windowed sliding window (e.g. last N attempts or last T seconds).
/// Forget ancient history.
///
/// ## Circuit breaker pattern
/// This is the key thing it's missing. A relay shouldn't just "lose points" on failure - it
/// should be removed from consideration temporarily. The states are:
///
/// - Closed (healthy, use it)
/// - Open (failed too many times recently, don't even try, back off)
/// - Half-open (backoff expired, send one probe request to test it)
///
/// The backoff when open should be exponential - first failure: wait 5s, then 30s, then 2min, etc.,
/// capped at something like 30min. This prevents hammering dead relays.
///
/// ## Selection strategy
///
/// Don't always pick the top-scored relay. That causes all traffic to pile onto one relay and you
/// never discover if lower-ranked ones have improved. Use a weighted random selection - higher
/// score = higher probability, but not guaranteed. Or split it: 80% go to top-3 by score, 20% are
/// exploratory probes to re-evaluate others.
///
/// ## What to Track (Relay)
///
/// - url / address
/// - current circuit state (closed / open / half-open)
/// - backoff_until: timestamp
/// - latency_samples: rolling buffer of last N latency values
/// - attempts: count in current window
/// - successes: count in current window
/// - consecutive_failures: reset on any success
/// - last_success: timestamp
/// - last_attempt: timestamp
///
/// ## Flow on connection attempt
///
/// 1. Filter out relays where circuit is open and backoff_until is in the future
/// 2. Promote open relays to half-open if their backoff has expired
/// 3. Score remaining relays, select by weighted random
/// 4. Attempt connection, measure latency
/// 5. On success: record latency, increment success, reset consecutive failures, set circuit to
///    closed
/// 6. On failure: increment consecutive failures, if threshold exceeded open the circuit and set
///    exponential backoff
impl Relay {
    pub fn info(&self) -> Result<RelayInfo> {
        let conn = NETWORK_DB.lock();

        conn.query_row("SELECT * FROM relays WHERE id = ?1", params![self.id.as_ref()], |row| {
            Ok(RelayInfo {
                id:                   row.get("id")?,
                host:                 row.get("host")?,
                port:                 row.get::<_, i64>("port")? as u16,
                circuit_state:        {
                    let s: String = row.get("circuit_state")?;
                    CircuitState::try_from(s)
                        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?
                },
                consecutive_failures: row.get::<_, i64>("consecutive_failures")? as u32,
                window_attempts:      row.get::<_, i64>("window_attempts")? as u32,
                window_successes:     row.get::<_, i64>("window_successes")? as u32,
                last_latency:         row.get::<_, Option<i64>>("last_latency")?.map(|v| v as u64),
                last_seen:            row.get::<_, i64>("last_seen")? as u64,
                last_connect:         row.get::<_, Option<i64>>("last_connect")?.map(|v| v as u64),
            })
        })
        .map_err(|e| anyhow!(e))
    }

    /// Selects a relay via weighted random selection.
    ///
    /// Eligible: closed, half_open, or open with expired backoff (promoted to half_open).
    /// Score: weighted composite of success rate and normalized latency.
    /// Selection: 80% weighted random from top-3, 20% uniform exploratory from the rest.
    pub fn fetch_best() -> Result<Self, RelayError> {
        let conn = NETWORK_DB.lock();
        let now = systime().as_millis() as i64;

        conn.execute("BEGIN", [])?;

        conn.execute(
            "UPDATE relays SET circuit_state = 'half_open'
             WHERE circuit_state = 'open'
               AND backoff_until IS NOT NULL
               AND backoff_until <= ?1",
            params![now],
        )?;

        struct Candidate {
            id:           String,
            host:         String,
            port:         u16,
            latency:      Option<i64>,
            success_rate: f64,
        }

        let mut stmt = conn.prepare(
            "SELECT id, host, port,
                    last_latency,
                    CAST(window_successes AS REAL) / MAX(window_attempts, 1) AS success_rate,
                    MIN(last_latency) OVER () AS min_lat,
                    MAX(last_latency) OVER () AS max_lat
             FROM relays
             WHERE protocol_version = ?1
               AND circuit_state IN ('closed', 'half_open')",
        )?;

        let rows: Vec<Candidate> = stmt
            .query_map(params![PROTOCOL_VERSION], |row| {
                let latency: Option<i64> = row.get(3)?;
                let success_rate: f64 = row.get(4)?;

                Ok(Candidate {
                    id: row.get(0)?,
                    host: row.get(1)?,
                    port: row.get::<_, i64>(2)? as u16,
                    latency,
                    success_rate,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        conn.execute("COMMIT", [])?;

        if rows.is_empty() {
            return Err(RelayError::NoneAvailable);
        }

        // Compute composite score for each candidate
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
            // Exploratory: uniform random from outside top-N
            let tail = &scored[TOP_N..];
            let idx = (rand::random::<f64>() * tail.len() as f64) as usize;
            tail[idx.min(tail.len() - 1)].1
        } else {
            // Exploitation: weighted random from top-N
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

        Ok(Self {
            id:         Arc::from(chosen.id.as_str()),
            host:       Arc::from(chosen.host.as_str()),
            port:       chosen.port,
            connection: None,
        })
    }

    /// Upserts relays from a resolver response.
    ///
    /// Only updates addressing and version — does not touch circuit state or window stats.
    pub fn refresh(relays: &[RelayDescriptor]) -> Result<(), RelayError> {
        let conn = NETWORK_DB.lock();
        let now = systime().as_millis() as u64;

        let mut stmt = conn.prepare(
            "INSERT INTO relays (id, host, port, last_seen, protocol_version, window_start)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               host             = excluded.host,
               port             = excluded.port,
               last_seen        = excluded.last_seen,
               protocol_version = excluded.protocol_version",
        )?;

        for r in relays {
            stmt.execute(params![
                r.id.to_string(),
                r.addr.ip().to_string(),
                r.addr.port(),
                now,
                PROTOCOL_VERSION,
                now,
            ])?;
        }

        Ok(())
    }

    /// Records a successful connection. Closes circuit, resets failure streak,
    /// stores latency, rolls the stats window if expired, trims sample history.
    pub fn record_success(&self, latency_ms: u64) -> Result<(), RelayError> {
        let conn = NETWORK_DB.lock();
        let now = systime().as_millis() as i64;
        let window_threshold = now - WINDOW_DURATION_MS as i64;

        conn.execute(
            "UPDATE relays SET
                   circuit_state        = 'closed',
                   backoff_until        = NULL,
                   consecutive_failures = 0,
                   last_latency         = ?1,
                   last_connect         = ?2,
                   window_attempts      = CASE WHEN window_start < ?3 THEN 1 ELSE window_attempts + 1 END,
                   window_successes     = CASE WHEN window_start < ?3 THEN 1 ELSE window_successes + 1 END,
                   window_start         = CASE WHEN window_start < ?3 THEN ?2 ELSE window_start END
                 WHERE id = ?4",
            params![latency_ms as i64, now, window_threshold, self.id.as_ref()],
        )?;

        conn.execute(
            "INSERT INTO relay_latency_samples (relay_id, measured_at, latency)
         VALUES (?1, ?2, ?3)",
            params![self.id.as_ref(), now, latency_ms as i64],
        )?;

        // Trim to last LATENCY_SAMPLE_LIMIT samples using rowid to handle duplicate timestamps
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

    /// Records a failed connection attempt.
    ///
    /// After FAILURE_THRESHOLD consecutive failures the circuit opens with
    /// exponential backoff: 3 → 5s, 4 → 10s, 5 → 20s, … capped at 30m.
    pub fn record_failure(&self) -> Result<(), RelayError> {
        let conn = NETWORK_DB.lock();
        let now = systime().as_millis() as i64;
        let window_threshold = now - WINDOW_DURATION_MS as i64;

        let consecutive_failures: u32 = conn.query_row(
            "UPDATE relays SET
                   consecutive_failures = consecutive_failures + 1,
                   last_failure         = ?1,
                   window_attempts      = CASE WHEN window_start < ?3 THEN 1 ELSE window_attempts + 1 END,
                   window_start         = CASE WHEN window_start < ?3 THEN ?1 ELSE window_start END
                 WHERE id = ?2
                 RETURNING consecutive_failures",
            params![now, self.id.as_ref(), now as i64, window_threshold],
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

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: RESOLVE :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

impl Relay {
    /// Resolves relays by connecting to one of the resolver seeds provided.
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

                Relay::refresh(&relays)?;
                conn.close(quinn::VarInt::from_u32(1), &[]);

                break Ok(());
            }
        }
    }
}

// // // // // // // // // // // // // // // // // //
