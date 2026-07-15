use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeId;
use common::quic::id::NodeKey;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;
use quinn::ServerConfig;

use common::warn;

use crate::config::AppConfig;
use crate::fcm::FcmSender;
use crate::registry::PushRegistry;

/// The push gateway node: a blind QUIC listener that registers `P → token` and
/// dispatches wake requests to the platform push service.
pub struct Gateway {
    pub endpoint: Arc<Endpoint>,
    pub registry: PushRegistry,
    /// `None` when no service-account is configured — the gateway still serves
    /// registrations, but FCM wakes are dropped.
    pub fcm:      Option<FcmSender>,
}

impl Gateway {
    fn get_server_cfg(cfg: &AppConfig) -> Result<ServerConfig> {
        setup_crypto_provider()?;
        use ProtoRole as PR;
        // Dialers: devices register over `client/1`, home relays wake over
        // `relay/1`. The gateway itself never dials anyone over QUIC (FCM is
        // HTTPS), so it needs no ALPN role of its own.
        build_server_cfg(&cfg.network.cert_path, &cfg.network.key_path, &[PR::Client, PR::Relay])
    }

    fn endpoint(cfg: &AppConfig) -> Endpoint {
        let server_config = graceful!(Self::get_server_cfg(cfg), "building the TLS server config");
        let endpoint = graceful!(
            Endpoint::server(server_config, cfg.network.bind_addr()),
            "starting the QUIC endpoint"
        );
        if let Ok(addr) = endpoint.local_addr() {
            info!("gateway listening at QUIC({:?})", addr);
        }
        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let mut endpoint = Self::endpoint(&cfg);
        // Default client config so the gateway can dial the resolver (`relay/1`)
        // to register itself.
        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "loading the root CA");
        let client_cfg =
            graceful!(build_client_cfg(ProtoRole::Relay, &roots), "building the client config");
        endpoint.set_default_client_config(client_cfg);
        let endpoint = Arc::new(endpoint);

        // Load our identity key: for the IPK log line and to sign GatewayHello.
        let signing = secret_from_key(&cfg.network.key_path).ok();
        if let Some(s) = &signing {
            if let Ok(key) = NodeKey::new(s.verifying_key()) {
                info!("initializing gateway with IPK({})", key.key());
            }
        }

        // Register with the resolver so relays can discover us.
        if let Some(signing) = signing {
            let node_id = NodeId::new(signing.verifying_key().to_bytes());
            let seeds = cfg.resolver.as_ref().map(|r| r.seed.clone()).unwrap_or_default();
            crate::resolver_link::spawn((*endpoint).clone(), seeds, signing, node_id);
        }

        let fcm = cfg.push.fcm_service_account.as_deref().and_then(|path| {
            match FcmSender::from_service_account(path) {
                Ok(sender) => {
                    info!("FCM dispatch enabled (project {})", sender.project_id());
                    Some(sender)
                },
                Err(e) => {
                    warn!("FCM disabled — could not load service-account: {e:#}");
                    None
                },
            }
        });

        Self { endpoint, registry: PushRegistry::default(), fcm }
    }
}
