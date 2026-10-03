use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;

use crate::proto::RelayId;
use crate::types::bytes::Bytes;

/// Cap on `count_xor_near + count_rtt_near` (saturating), so an unauthenticated query stays cheap.
pub const MAX_BOOTSTRAP_RESULTS: u8 = 32;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayDescriptor {
    pub id:     RelayId,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr:   SocketAddr,
    /// Full Ed25519 identity key; a relay dialing another checks the leaf cert's SPKI against it.
    pub pubkey: Bytes<32>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    GetRelays(),
    /// Unauthenticated: the answer is a reranked subset of [`ClientRequest::GetRelays`].
    GetBootstrapPeers {
        /// XOR pivot, not authenticated against the connection.
        near:           [u8; 32],
        count_xor_near: u8,
        /// Ranked by heartbeat recency, a proxy for RTT.
        count_rtt_near: u8,
    },

    /// Unauthenticated directory; dialers check `PUSH_GATEWAY` on the gateway's cert. Appended
    /// last (postcard variant order).
    GetGateways(),

    /// Public, from operator configuration. Appended last (postcard variant order).
    GetStores(),
}

pub type GatewayDescriptor = RelayDescriptor;

/// Store ids are permanent references in messages. Update `base_url` to move a store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoreDescriptor {
    pub id:       u16,
    /// Scheme + host (+ port), no trailing slash: `https://s1.promtuz.app`.
    pub base_url: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientResponse {
    GetRelays { relays: Vec<RelayDescriptor> },
    /// Two rankings, not deduplicated: by XOR distance from `near`, and most recently active first.
    GetBootstrapPeers {
        xor_near: Vec<RelayDescriptor>,
        rtt_near: Vec<RelayDescriptor>,
    },
    GetGateways { gateways: Vec<GatewayDescriptor> },
    GetStores { stores: Vec<StoreDescriptor> },
}
