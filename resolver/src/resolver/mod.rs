use std::collections::HashMap;
use std::process;
use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::proto::RelayId;
use common::proto::ResolverId;
use common::proto::relay_res::LifetimeP;
use common::quic::CloseReason;
use common::quic::config::build_server_cfg;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeId;
use common::quic::id::derive_node_id;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use quinn::Connection;
use quinn::Endpoint;
use quinn::ServerConfig;
use tokio::sync::Mutex;

use crate::resolver::relays::RelayEntry;
use crate::util::config::AppConfig;
use crate::util::systime;

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

    fn id(cfg: &AppConfig) -> NodeId {
        let secret = match secret_from_key(&cfg.network.key_path) {
            Ok(sec) => sec,
            Err(_) => process::exit(0),
        };

        derive_node_id(&secret.public_key())
    }

    fn endpoint(cfg: &AppConfig) -> Endpoint {
        let server_config = graceful!(Self::get_server_cfg(cfg), "failed to setup server config:");
        let endpoint = graceful!(
            Endpoint::server(server_config, cfg.network.address),
            "failed to start quic server:"
        );

        if let Ok(addr) = endpoint.local_addr() {
            info!("resolver listening at QUIC({:?})", addr);
        }

        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let id = Self::id(&cfg);

        info!("initializing resolver with ID({id})");

        Self { id, endpoint: Arc::new(Self::endpoint(&cfg)), relays: HashMap::new(), cfg }
    }

    /// Will return [HelloAck] if registered succesfully
    ///
    /// Returns [ConnectionError] instead of relay already exists
    pub fn register_relay(
        &mut self, conn: Arc<Connection>, hello: &LifetimeP,
    ) -> Result<LifetimeP, CloseReason> {
        let LifetimeP::RelayHello { relay_id, .. } = *hello else {
            return Err(CloseReason::PacketMismatch);
        };

        if let Some(existing) = self.relays.remove(&relay_id) {
            let close = CloseReason::DuplicateConnect;
            existing.conn.close(close.code(), &close.reason());
            // can toggle behavior by uncommenting this err return and commenting out previous 2
            // lines return Err(CloseReason::AlreadyConnected);
        }

        self.relays.insert(relay_id, RelayEntry { id: relay_id, conn });

        let hello_ack = LifetimeP::HelloAck { resolver_time: systime().as_millis() };

        Ok(hello_ack)
    }

    /// Closes resolver
    pub fn close(&self) {
        self.relays.iter().for_each(|(_, r)| {
            r.conn.close(CloseReason::ShuttingDown.code(), b"ResolverShuttingDown");
        });
    }
}
