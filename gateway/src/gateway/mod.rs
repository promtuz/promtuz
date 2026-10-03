use common::graceful;
use common::info;
use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca;
use common::quic::protorole::ProtoRole;
use common::server::accept::quota;
use common::server::daemon;
use common::warn;
use quinn::Endpoint;

use crate::config::AppConfig;
use crate::fcm::FcmSender;
use crate::registry::PushRegistry;
use crate::store::StickerStore;

pub struct Gateway {
    pub endpoint: Endpoint,
    pub registry: PushRegistry,
    pub fcm:      Option<FcmSender>,
    /// Keyed by pseudonym: whoever holds a P, its home relay or anyone who learnt it, gets no
    /// more wakes than a relay would send.
    pub wakes:    WakeLimiter,
    pub store:    Option<StickerStore>,
}

pub type WakeLimiter = governor::RateLimiter<
    [u8; 32],
    governor::state::keyed::DefaultKeyedStateStore<[u8; 32]>,
    governor::clock::DefaultClock,
>;

/// Mirrors the relay's own per-recipient wake budget of 120 an hour.
const MAX_WAKES_PER_P_PER_MIN: u32 = 2;
const MAX_WAKE_BURST: u32 = 12;

impl Gateway {
    pub fn new(cfg: AppConfig) -> Self {
        use ProtoRole as PR;
        // Devices dial over the client ALPN, home relays over the relay ALPN.
        let mut endpoint = daemon::bind(&cfg.network, &[PR::Client, PR::Relay], "gateway");
        // The gateway registers with the resolver under the relay ALPN.
        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "loading the root CA");
        let client_cfg =
            graceful!(build_client_cfg(PR::Relay, &roots), "building the client config");
        endpoint.set_default_client_config(client_cfg);

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

        let store = cfg.store.as_ref().and_then(|s| match StickerStore::from_config(s) {
            Ok(store) => Some(store),
            Err(e) => {
                warn!("sticker store disabled — {e:#}");
                None
            },
        });

        Self {
            endpoint,
            registry: graceful!(PushRegistry::open(&cfg.push.db), "opening the push registry"),
            fcm,
            wakes: governor::RateLimiter::keyed(quota(MAX_WAKES_PER_P_PER_MIN, MAX_WAKE_BURST)),
            store,
        }
    }
}
