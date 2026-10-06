use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use common::graceful;
use common::info;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg_with_alpn_split;
use common::quic::config::load_root_ca;
use common::quic::id::NodeId;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use parking_lot::RwLock;
use quinn::Connection;
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TokioRuntime;

use crate::dht::Dht;
use crate::storage::db::Store;
use crate::util::config::AppConfig;

pub type RelayRef = Arc<Relay>;

#[derive(Debug)]
pub struct Relay {
    pub endpoint: Endpoint,

    /// Peeled STUN/TURN datagrams, taken once by `main` for `stunturn::serve`.
    pub assist: Mutex<Option<crate::stunturn::AssistInbox>>,
    pub assist_enabled: bool,

    pub turn: Option<Arc<crate::turn::Turn>>,

    pub cfg: AppConfig,

    pub store: Arc<Store>,

    pub dht: Option<Arc<Dht>>,
    /// MLS custody is always available. With DHT disabled this context has no peers or dialer;
    /// with DHT enabled it is shared with the mesh, including its storage and rate limiters.
    pub(crate) mls: Arc<Dht>,
    /// Storage identity exists even when distributed routing is disabled.
    pub node_id: NodeId,

    /// Authenticated clients by IPK, shared with `Dht` for home-side delivery.
    pub clients: Arc<RwLock<HashMap<[u8; 32], Connection>>>,

    /// Subscriber IPK to its interest set, removed on disconnect. Consent is stored separately.
    pub presence_subs: RwLock<HashMap<[u8; 32], HashSet<[u8; 32]>>>,
    pub presence_leases: Arc<RwLock<HashMap<[u8; 32], common::proto::dht_p2p::PresenceLease>>>,
    pub presence_versions: RwLock<HashMap<[u8; 32], u64>>,

    /// Foreground-active clients and when they asserted it; connection alone is not presence.
    pub active_clients: RwLock<HashMap<[u8; 32], u64>>,
    // ponytail: stripes bound lock memory; split by owner if collisions limit throughput.
    presence_updates: [tokio::sync::Mutex<()>; 64],
}

impl Relay {
    pub async fn presence_update(&self, owner: &[u8; 32]) -> tokio::sync::MutexGuard<'_, ()> {
        self.presence_updates[usize::from(owner[0]) % self.presence_updates.len()].lock().await
    }
    /// The peer ALPN serves a self-signed NodeKey cert that libcore pins against
    /// `RelayDescriptor.pubkey`; every other ALPN serves the CA-issued cert.
    pub fn bind(
        cfg: &AppConfig, node_signing: &SigningKey,
    ) -> (Endpoint, Option<crate::stunturn::AssistInbox>) {
        use ProtoRole as PR;

        // Only roles `Handler::handle` serves; rustls rejects any other ALPN.
        let server_cfg = graceful!(
            build_server_cfg_with_alpn_split(
                &cfg.network.cert_path,
                &cfg.network.key_path,
                node_signing.clone(),
                &[PR::Peer, PR::Client],
            ),
            "building the TLS server config"
        );

        let std_sock =
            graceful!(std::net::UdpSocket::bind(cfg.network.bind_addr()), "binding the QUIC socket");

        // With assist on, a socket wrapper peels STUN/TURN datagrams off the QUIC port.
        let (endpoint, assist) = if cfg.assist.enabled {
            let (socket, assist) =
                graceful!(crate::stunturn::wrap_socket(std_sock), "wrapping the QUIC socket");
            let endpoint = graceful!(
                Endpoint::new_with_abstract_socket(
                    EndpointConfig::default(),
                    Some(server_cfg),
                    socket,
                    Arc::new(TokioRuntime),
                ),
                "starting the QUIC endpoint"
            );
            (endpoint, Some(assist))
        } else {
            let endpoint = graceful!(
                Endpoint::new(
                    EndpointConfig::default(),
                    Some(server_cfg),
                    std_sock,
                    Arc::new(TokioRuntime),
                ),
                "starting the QUIC endpoint"
            );
            (endpoint, None)
        };

        if let Ok(addr) = endpoint.local_addr() {
            info!("relay listening at QUIC({:?})", addr);
        }
        (endpoint, assist)
    }

    /// `signing` is the node key: both the relay's identity and its TLS key. The endpoint and the
    /// assist inbox come from [`Relay::bind`].
    pub fn new(
        cfg: AppConfig, signing: &SigningKey,
        (mut endpoint, assist): (Endpoint, Option<crate::stunturn::AssistInbox>), store: Arc<Store>,
    ) -> Self {
        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "loading the root CA");

        let client_cfg = graceful!(
            build_client_cfg(ProtoRole::Relay, &roots),
            "building the QUIC client config"
        );
        // The peer ALPN trusts self-signed NodeKey certs pinned to the dialed NodeId, not the CA.
        let peer_client_cfg = Arc::new(graceful!(
            crate::dht::peer_dial::build_peer_client_cfg(),
            "building the peer client config"
        ));

        endpoint.set_default_client_config(client_cfg);

        let clients = Arc::new(RwLock::new(HashMap::new()));
        let presence_leases = Arc::new(RwLock::new(HashMap::new()));

        let node_id = NodeId::new(signing.verifying_key());
        let mut d = Dht::new(node_id, signing.clone(), cfg.dht.clone(), store.clone());
        if cfg.dht.enabled {
            d.attach_dialer(endpoint.clone(), peer_client_cfg.clone());
            d.attach_clients(clients.clone());
            d.attach_presence_leases(presence_leases.clone());
            info!("DHT enabled (node_id = {node_id})");
        }
        let mls = Arc::new(d);
        let dht = cfg.dht.enabled.then(|| mls.clone());

        let assist_enabled = cfg.assist.enabled;
        let turn = cfg.turn.enabled.then(|| {
            Arc::new(graceful!(crate::turn::Turn::bind(&cfg.turn, signing), "starting TURN"))
        });
        Self {
            cfg,
            store,
            dht,
            mls,
            node_id,
            endpoint,
            assist: Mutex::new(assist),
            assist_enabled,
            turn,
            clients,
            presence_subs: RwLock::new(HashMap::new()),
            presence_leases,
            presence_versions: RwLock::new(HashMap::new()),
            active_clients: RwLock::new(HashMap::new()),
            presence_updates: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
        }
    }
}
