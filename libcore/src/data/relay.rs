use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::RelayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use log::info;
use quinn::Connection;
use quinn::VarInt;
use rusqlite::params;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::data::ResolverSeed;
use crate::db::network::NETWORK_DB;
use crate::db::network::RelayRow;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::quic::dialer::DialerError;
use crate::quic::dialer::connect_to_any_seed;
use crate::quic::dialer::quinn_err;
use crate::utils::systime;

/// Shareable Statistical Data
#[derive(Debug, Serialize)]
pub struct RelayInfo {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub reputation: i32,
    pub avg_latency: Option<u64>,
}

/// Relay instance
#[derive(Debug, Clone)]
pub struct Relay {
    pub id: Arc<str>,
    pub host: Arc<str>,
    pub port: u16,

    /// Contains quinn connection IF connected
    pub connection: Option<Connection>,
}

#[derive(Error, Debug)]
pub enum ResolveError {
    #[error("resolver did not return any relay")]
    EmptyResponse,

    #[error("dialer error: {0}")]
    DialerError(#[from] DialerError),

    #[error("failed to unpack: {0}")]
    UnpackError(#[from] anyhow::Error),
}

/// TODO: Create unit testing for this
impl Relay {
    pub fn info(&self) -> Result<RelayInfo> {
        let conn = NETWORK_DB.lock();

        conn.query_one("SELECT * FROM relays WHERE id = ?1", [self.id.clone()], |row| {
            Ok(RelayInfo {
                id: row.get("id")?,
                host: row.get("host")?,
                port: row.get("port")?,
                avg_latency: row.get("last_avg_latency")?,
                reputation: row.get("reputation")?,
            })
        })
        .map_err(|e| anyhow!(e))
    }

    /// "Best" how?
    ///
    /// - Must match current version
    /// - Lowest last avg latency if exists
    /// - Lowest last seen
    /// - Lowest last connect if exists
    pub fn fetch_best() -> rusqlite::Result<Self> {
        let conn = NETWORK_DB.lock();

        conn.query_row(
            "SELECT * FROM relays 
                  WHERE 
                    last_version = ?1 AND
                    reputation >= 0
                  ORDER BY 
                      reputation DESC,
                      last_seen DESC, 
                      last_connect DESC, 
                      last_avg_latency ASC 
                  LIMIT 1",
            [PROTOCOL_VERSION],
            RelayRow::from_row,
        )
        .map(|r| Self {
            id: r.id.into(),
            host: r.host.into(),
            port: r.port,
            connection: None,
        })
    }

    pub fn refresh(relays: &[RelayDescriptor]) -> Result<u8> {
        let conn = NETWORK_DB.lock();

        // Increase reputation as resolver says so
        let mut stmt = conn.prepare(
            "INSERT INTO relays (
                    id, host, port, last_seen, last_version
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                    host         = excluded.host,
                    port         = excluded.port,
                    last_seen    = excluded.last_seen,
                    last_version = excluded.last_version,
                    reputation   = reputation + 1",
        )?;

        relays.iter().for_each(|r| {
            _ = stmt.execute((
                r.id.to_string(),
                r.addr.ip().to_string(),
                r.addr.port(),
                systime().as_millis() as u64,
                PROTOCOL_VERSION,
            ));
        });

        Ok(0)
    }

    /// Resolves relays by connected to one of the resolver seed provided
    ///
    /// Any type of failure except ui related is not tolerated and will return an error
    pub async fn resolve(seeds: &[ResolverSeed]) -> Result<(), ResolveError> {
        use ConnectionState as CS;

        CS::Resolving.emit();

        let conn = connect_to_any_seed(seeds).await.inspect_err(|_| CS::Failed.emit())?;

        let req = ClientRequest::GetRelays().pack().unwrap();

        let (mut send, mut recv) = conn.open_bi().await.map_err(quinn_err)?;
        send.write_all(&req).await.map_err(quinn_err)?;
        send.flush().await.map_err(quinn_err)?;

        use ClientResponse;
        use ClientResponse::*;

        loop {
            let client_resp = ClientResponse::unpack(&mut recv).await?;

            #[allow(irrefutable_let_patterns)]
            if let GetRelays { relays } = client_resp {
                if relays.is_empty() {
                    break Err(ResolveError::EmptyResponse);
                }

                Relay::refresh(&relays)?;
                conn.close(VarInt::from_u32(1), &[]);

                break Ok(());
            }
        }
    }

    /// Reduces reputation of relay by 1
    ///
    /// Returns updated reputation
    pub fn downvote(&self) -> anyhow::Result<i16> {
        info!("INFO: downvoting relay({})", self.id);
        let conn = NETWORK_DB.lock();

        Ok(conn.query_one(
            "UPDATE relays SET reputation = reputation - 1 WHERE id = ?1 RETURNING reputation;",
            params![self.id],
            |r| r.get(0),
        )?)
    }

    /// Increases reputation of relay by 1
    ///
    /// Returns updated reputation
    pub fn upvote(&self) -> anyhow::Result<i16> {
        info!("INFO: upvoting relay({})", self.id);
        let conn = NETWORK_DB.lock();

        Ok(conn.query_one(
            "UPDATE relays SET reputation = reputation + 1 WHERE id = ?1 RETURNING reputation;",
            params![self.id],
            |r| r.get(0),
        )?)
    }
}
