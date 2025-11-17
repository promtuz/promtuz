use std::collections::HashMap;
use std::process;
use std::sync::Arc;

use anyhow::Result;
use common::msg::RelayId;
use common::msg::ResolverId;
use common::msg::reason::CloseReason;
use common::msg::resolver::HelloAck;
use common::msg::resolver::RelayHello;
use common::quic::config::build_server_cfg;
use common::quic::config::setup_crypto_provider;
use common::quic::id::derive_id;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use quinn::Connection;
use quinn::Endpoint;
use quinn::ServerConfig;
use tokio::sync::Mutex;

use crate::graceful;
use crate::resolver::relays::RelayEntry;
use crate::util::config::AppConfig;
use crate::util::systime_sec;

pub mod relays;
pub mod rpc;

pub type ResolverRef = Arc<Mutex<Resolver>>;

/// Represents a single resolver node in the network but locally
///
/// contains all necessary information instead of a global state
#[derive(Debug)]
pub struct Resolver {
    pub id: ResolverId,
    pub cfg: AppConfig,
    pub endpoint: Arc<Endpoint>,
    relays: HashMap<RelayId, RelayEntry>,
}

impl Resolver {
    fn get_server_cfg(cfg: &AppConfig) -> Result<ServerConfig> {
        setup_crypto_provider()?;
        use ProtoRole as PR;
        build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &[PR::Resolver, PR::Relay, PR::Client],
        )
    }

    fn id(cfg: &AppConfig) -> String {
        let secret = match secret_from_key(&cfg.network.key_path) {
            Ok(sec) => sec,
            Err(err) => {
                eprintln!("RESOLVER_ERR: {}", err);
                process::exit(0);
            },
        };

        derive_id(&secret.public_key())
    }

    fn endpoint(cfg: &AppConfig) -> Endpoint {
        let server_config = graceful!(Self::get_server_cfg(cfg), "CONFIG_ERR:");
        let endpoint = graceful!(Endpoint::server(server_config, cfg.network.address), "QUIC_ERR:");

        if let Ok(addr) = endpoint.local_addr() {
            println!("QUIC(RESOLVER): listening at {:?}", addr);
        }

        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        Self {
            id: Self::id(&cfg),
            endpoint: Arc::new(Self::endpoint(&cfg)),
            relays: HashMap::new(),
            cfg,
        }
    }

    /// Will return [HelloAck] if registered succesfully
    ///
    /// Returns [ConnectionError] instead of relay already exists
    pub fn register_relay(
        &mut self, conn: Arc<Connection>, hello: &RelayHello,
    ) -> Result<HelloAck, CloseReason> {
        if let Some(existing) = self.relays.remove(&hello.relay_id) {
            let close = CloseReason::DuplicateConnect;
            existing.conn.close(close.code(), &close.reason());
            // can toggle behavior by uncommenting this err return and commenting out previous line
            // return Err(CloseReason::AlreadyConnected);
        }

        println!("RELAY_CONNECT: ID({})", hello.relay_id);

        self.relays.insert(hello.relay_id.clone(), RelayEntry { id: hello.relay_id.clone(), conn });

        let jitter = (rand::random::<f32>() * 2000.0 - 1000.0) as i32;

        let hello_ack = HelloAck {
            accepted: true,
            interval_heartbeat_ms: (25 * 1000 + jitter).max(0) as u32,
            reason: None,
            resolver_time: systime_sec(),
        };

        Ok(hello_ack)
    }

    /// Closes resolver
    pub fn close(&self) {
        self.relays.iter().for_each(|(_, r)| {
            r.conn.close(CloseReason::ShuttingDown.code(), b"ResolverShuttingDown");
        });
    }
}
