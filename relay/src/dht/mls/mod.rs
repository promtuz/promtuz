//! MLS stash relay — the home-relay side of KeyPackage and Welcome
//! storage, plus the originate half that fans a phone's `client/0`
//! wrapper RPC out to the K storage homes over `peer/1`.
//!
//! - [`kp`] / [`welcome`]: inbound `peer/1` handlers + fjall storage
//!   for the two stashes (`dht_keypackage`, `dht_welcome`).
//! - [`kp_originate`] / [`welcome_originate`]: the home relay acting
//!   as originator on behalf of an authenticated client.
//! - [`fanout`]: shared K-closest fan-out primitives.

pub(crate) mod fanout;
pub(crate) mod kp;
pub(crate) mod kp_originate;
pub(crate) mod welcome;
pub(crate) mod welcome_originate;

use common::quic::id::NodeId;

/// 32-byte stash key for `(domain, ipk)`: `BLAKE3(domain || ipk)`.
///
/// The short literal `domain` (`b"kp:"` / `b"welcome:"`) namespaces the
/// two stashes away from each other inside the unified 32-byte DHT
/// keyspace, so the routing layer doesn't need to know which
/// sub-namespace it's serving. Implemented via [`NodeId::new`] (BLAKE3
/// of the input) so the relay doesn't need a direct `blake3` dep.
pub fn stash_prefix(domain: &[u8], ipk: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(domain.len() + ipk.len());
    buf.extend_from_slice(domain);
    buf.extend_from_slice(ipk);
    *NodeId::new(&buf).as_bytes()
}
