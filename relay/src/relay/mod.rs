use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg_with_alpn_split;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeKey;
use common::quic::p256::secret_from_key_or_create;
use common::quic::protorole::ProtoRole;
use common::warn;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;

use crate::dht::Dht;
use crate::storage::db::Store;
use crate::util::config::AppConfig;

/// The relay's single Ed25519 keypair: identity **and** TLS.
///
/// One key signs application-layer messages (`RelayHello` to the resolver),
/// derives the public-facing `relay_id`, backs the in-memory `peer/1`
/// self-signed cert, and is the key the CA-issued cert certifies. Loaded
/// from `key_path`; auto-created `0o600` on first boot if absent.
#[derive(Debug)]
pub struct RelayKeys {
    pub signing: SigningKey,
    pub public:  VerifyingKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self, ()> {
        let secret = secret_from_key_or_create(&cfg.network.key_path)?;
        let public = secret.verifying_key();

        Ok(Self { signing: secret, public })
    }
}

pub type RelayRef = Arc<Relay>;

/// Represents a single relay node running in the network.
///
/// It's *local identity* of the relay process,
/// not a message exchanged over the wire.
///
/// It's apparently like a core process handler
#[derive(Debug)]
pub struct Relay {
    pub key: NodeKey,

    /// Long-lived Ed25519 identity keypair. Held here so things that need to
    /// sign on this relay's behalf (e.g. `RelayHello` to the resolver) don't
    /// have to re-read the on-disk PKCS#8 file every time.
    pub keys: RelayKeys,
    /// SystemTime in ms since EPOCH when relay is started first
    // pub start_ms: u128,
    pub endpoint: Endpoint,

    pub cfg: AppConfig,

    pub client_cfg: Arc<ClientConfig>,

    pub store: Arc<Store>,

    /// Shared DHT runtime state. `None` when `cfg.dht.enabled = false`;
    /// every code path that would touch the DHT checks the option first
    /// and falls through to the pre-DHT behaviour.
    pub dht: Option<Arc<Dht>>,

    /// Connected + authenticated clients, keyed by IPK.
    ///
    /// `Arc<RwLock<...>>` (rather than a bare `RwLock`) so the inner
    /// map can be shared with the DHT's `Dht::clients` reference for
    /// the home-side `Forward` handler. The relay-side
    /// per-client handler in `quic/handler/client/mod.rs` and the DHT
    /// home-side handler in `dht/forward.rs::handle_forward_rpc` both
    /// observe the same map; cloning the `Arc` is cheap and avoids a
    /// back-pointer from `Dht` → `Relay`.
    pub clients: Arc<RwLock<HashMap<[u8; 32], Connection>>>,
}

impl Relay {
    /// Build the relay's QUIC endpoint with an ALPN-split server config:
    /// the peer/1 ALPN gets a NodeKey-bound self-signed Ed25519 cert
    /// (so libcore can pin SPKI against `RelayDescriptor.pubkey`),
    /// every other ALPN keeps the operator's CA-issued cert for the
    /// existing trust chain.
    fn endpoint(cfg: &AppConfig, node_signing: &SigningKey) -> Endpoint {
        use ProtoRole as PR;

        graceful!(setup_crypto_provider(), "installing the crypto provider");

        let server_cfg = graceful!(
            build_server_cfg_with_alpn_split(
                &cfg.network.cert_path,
                &cfg.network.key_path,
                node_signing.clone(),
                &[PR::Resolver, PR::Relay, PR::Peer, PR::Client],
            ),
            "building the TLS server config"
        );

        let endpoint = graceful!(Endpoint::server(server_cfg, cfg.network.address), "starting the QUIC endpoint");
        if let Ok(addr) = endpoint.local_addr() {
            info!("relay listening at QUIC({:?})", addr);
        }
        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let keys = RelayKeys::from_cfg(&cfg).expect("config failed");
        let key = NodeKey::new(keys.public).expect("invalid public key length");

        info!("initializing Relay with ID({key})");

        let mut endpoint = Self::endpoint(&cfg, &keys.signing);

        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "loading the root CA");

        let client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Relay, &roots), "building the QUIC client config"));
        // peer/1 is the key-as-identity trust domain (self-signed NodeKey
        // certs, pinned to the dialed NodeId post-handshake), not the CA
        // hierarchy — so it gets its own verifier, not build_client_cfg.
        let peer_client_cfg =
            Arc::new(graceful!(crate::dht::peer_dial::build_peer_client_cfg(), "building the peer/1 client config"));

        endpoint.set_default_client_config((*client_cfg).clone());

        // Single shared `Arc<Store>` so the DHT replica and the message
        // queue point at the same on-disk store but live in separate
        // keyspaces.
        let store = Arc::new(graceful!(Store::open("db"), "opening the fjall store"));
        // `clients` is `Arc<RwLock<...>>` (not a bare `RwLock`) so the
        // inner map can be cloned-by-Arc into `Dht.clients` for the
        // home-side `Forward` handler.
        let clients = Arc::new(RwLock::new(HashMap::new()));

        // DHT construction is gated on `cfg.dht.enabled`. When disabled,
        // the field stays `None` and every consumer falls through to
        // the legacy code path.
        let dht = if cfg.dht.enabled {
            let node_id = key.id();
            match Dht::new(node_id, keys.signing.clone(), cfg.dht.clone(), store.clone()) {
                Ok(mut d) => {
                    // Wire the outbound-dial machinery so the lookup
                    // module can open `peer/1` connections to other
                    // relays.
                    d.attach_dialer(endpoint.clone(), peer_client_cfg.clone());
                    // Share the connected-clients map so the home-side
                    // `Forward` handler can deliver locally when the
                    // recipient is online here.
                    d.attach_clients(clients.clone());
                    info!("DHT enabled (node_id = {node_id})");
                    Some(Arc::new(d))
                },
                Err(err) => {
                    // Don't kill the process on DHT-init failure — the
                    // relay can still serve clients without it. Log
                    // loudly so an operator knows the DHT is dark.
                    warn!("DHT init failed, continuing without DHT: {err}");
                    None
                },
            }
        } else {
            None
        };

        Self {
            key,
            keys,
            cfg,
            client_cfg,
            store,
            dht,
            endpoint,
            clients,
        }
    }
}
