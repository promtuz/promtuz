//! Client to Resolver Proto

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;

use crate::proto::RelayId;
use crate::types::bytes::Bytes;

/// Hard cap on the *combined* number of relay descriptors a single
/// [`ClientRequest::GetBootstrapPeers`] may ask for.
///
/// The resolver rejects requests where `count_xor_near + count_rtt_near > MAX_BOOTSTRAP_RESULTS`
/// (after `u8`-saturating addition) so an
/// unauthenticated caller cannot trigger an unbounded sort/scan. The
/// combined cap is a small fraction of `MAX_RELAYS = 1024` (the registry
/// size cap on the resolver) — large enough that a fresh-joining relay
/// gets a useful seed set in one round-trip, small enough that the
/// per-request work stays trivial.
pub const MAX_BOOTSTRAP_RESULTS: u8 = 32;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayDescriptor {
    pub id:     RelayId,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr:   SocketAddr,
    /// Relay's full Ed25519 identity public key.
    ///
    /// Vended by the resolver because every code path that *uses* a
    /// `RelayDescriptor` to open a relay-to-relay QUIC connection needs
    /// to verify the leaf cert's SPKI against the relay's pubkey on
    /// first contact. The resolver already has this pubkey from
    /// authenticated `RelayHello` (`LifetimeP::RelayHello::pubkey`),
    /// so shipping it costs the resolver nothing.
    ///
    /// Existing libcore consumers (`libcore/src/data/relay.rs::refresh`)
    /// ignore the field — it only matters at the relay-to-relay edge.
    pub pubkey: Bytes<32>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    GetRelays(),
    /// DHT bootstrap query. Asks the resolver for two ranked slices of
    /// its registry: one XOR-close to `near` (the requesting relay's own
    /// NodeId — used to seed neighbouring k-buckets), one ranked by
    /// recency-of-liveness as a proxy for "well-positioned" relays.
    ///
    /// Auth: **none**. This is a public query — any peer holding the
    /// `client/1` ALPN may issue it. The response is a strict subset of
    /// the data already exposed by [`ClientRequest::GetRelays`]; the new
    /// RPC just supplies a smarter ranking.
    GetBootstrapPeers {
        /// Requester's NodeId. Used as the pivot for the XOR ranking; not
        /// authenticated against the connection.
        near:           [u8; 32],
        /// Number of descriptors to return XOR-close to `near`.
        /// `count_xor_near + count_rtt_near` must not exceed
        /// [`MAX_BOOTSTRAP_RESULTS`] (saturating addition); requests
        /// over the cap are rejected by the resolver.
        count_xor_near: u8,
        /// Number of descriptors to return by lowest *resolver-to-relay*
        /// liveness recency (proxy for RTT).
        count_rtt_near: u8,
    },

    /// Fetch the currently-registered push gateways. Auth: none (public
    /// directory lookup). A relay caches the result and dials one to send a
    /// wake, verifying the `PUSH_GATEWAY` capability on the gateway's cert at
    /// dial. Appended last (postcard variant order).
    GetGateways(),
}

/// A push gateway's directory entry — same wire shape as [`RelayDescriptor`]
/// (id to check the dialed NodeId, addr to dial, pubkey carried along).
pub type GatewayDescriptor = RelayDescriptor;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientResponse {
    /// Resolver's response to [ClientRequest::GetRelays]
    GetRelays { relays: Vec<RelayDescriptor> },
    /// Resolver's response to [`ClientRequest::GetBootstrapPeers`].
    ///
    /// Two parallel lists, intentionally not de-duplicated: a relay can
    /// legitimately appear in both rankings (close-by-XOR *and*
    /// recently-active). Callers dedupe on `id` before inserting into
    /// their routing table.
    GetBootstrapPeers {
        /// Up to `count_xor_near` descriptors, sorted ascending by XOR
        /// distance from the requester's `near` NodeId.
        xor_near: Vec<RelayDescriptor>,
        /// Up to `count_rtt_near` descriptors, sorted by liveness-recency
        /// proxy for RTT (most-recently-active first).
        rtt_near: Vec<RelayDescriptor>,
    },
    /// Resolver's response to [`ClientRequest::GetGateways`].
    GetGateways { gateways: Vec<GatewayDescriptor> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::RelayId;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;

    /// Sample descriptor used as table-stakes test data. Kept minimal so
    /// the round-trip checks isolate the codec contract from the
    /// descriptor's own fields (which already live in
    /// `RelayDescriptor`'s derived `PartialEq`).
    ///
    /// The pubkey is seeded from the same `seed` so the test is
    /// deterministic without depending on a random source.
    fn sample_descriptor(seed: u8) -> RelayDescriptor {
        let bytes = [seed; 32];
        RelayDescriptor {
            id:     RelayId::from_bytes(bytes),
            addr:   format!("127.0.0.{}:4242", seed.max(1)).parse().expect("valid socket addr"),
            pubkey: Bytes([seed.wrapping_add(1); 32]),
        }
    }

    #[test]
    fn client_request_get_bootstrap_peers_round_trips_through_postcard() {
        // Pin the wire shape of the new variant so a future field
        // reorder or rename surfaces here, not weeks later as silently
        // drifted decoded payloads. Counts use mid-range values so an
        // off-by-one in the codec for either field shows up.
        let near = [0xABu8; 32];
        let req = ClientRequest::GetBootstrapPeers { near, count_xor_near: 8, count_rtt_near: 4 };
        let bytes = req.ser().expect("postcard serialize");
        let decoded = ClientRequest::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn client_response_get_relays_round_trips_through_postcard() {
        let resp =
            ClientResponse::GetRelays { relays: vec![sample_descriptor(1), sample_descriptor(2)] };
        let bytes = resp.ser().expect("postcard serialize");
        let decoded = ClientResponse::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, resp);
    }

    #[test]
    fn client_response_get_bootstrap_peers_round_trips_through_postcard() {
        // Both lists populated with overlapping descriptors so the
        // round-trip exercises the `Vec<RelayDescriptor>` codec twice
        // and confirms the two fields don't share state in postcard's
        // internal representation.
        let resp = ClientResponse::GetBootstrapPeers {
            xor_near: vec![sample_descriptor(1), sample_descriptor(2)],
            rtt_near: vec![sample_descriptor(3)],
        };
        let bytes = resp.ser().expect("postcard serialize");
        let decoded = ClientResponse::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, resp);
    }

    #[test]
    fn client_response_get_bootstrap_peers_empty_lists_round_trip() {
        // The legitimate "brand-new network, no peers known" case
        // returns empty lists. Pin that postcard handles two
        // back-to-back zero-length `Vec`s correctly under the
        // length-prefix encoding.
        let resp = ClientResponse::GetBootstrapPeers { xor_near: vec![], rtt_near: vec![] };
        let bytes = resp.ser().expect("postcard serialize");
        let decoded = ClientResponse::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, resp);
    }
}
