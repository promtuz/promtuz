use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::quic::id::NodeId;
use serde::{Deserialize, Serialize};

use crate::dht::routing::{closest_distance, RoutingTable};
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;

pub mod routing;
#[cfg(test)]
mod tests;

/// Default Kademlia parameters (in-memory only for now)
#[derive(Debug, Clone)]
pub struct DhtParams {
    pub bucket_size: usize,
    pub k: usize,
    pub alpha: usize,
    pub user_ttl: Duration,
    pub republish_interval: Duration,
}

impl Default for DhtParams {
    fn default() -> Self {
        Self {
            bucket_size: 20,
            k: 8,
            alpha: 3,
            user_ttl: Duration::from_secs(5 * 60),
            republish_interval: Duration::from_secs(2 * 60),
        }
    }
}

/// Contact information for a relay participating in the DHT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeContact {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct UserMetadata {
    pub status: Option<String>,
    // pub capabilities: Vec<String>,
}

/// Presence information for a connected user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserRecord {
    /// Identity public key (Ed25519)
    #[serde(with = "serde_bytes")]
    pub ipk: [u8; 32],
    /// Relay currently serving the user
    pub relay: NodeId,
    /// Address of the serving relay (other relays can dial)
    pub relay_addr: SocketAddr,
    /// Unix seconds
    pub timestamp: u64,
    /// Optional user supplied signature over the record
    #[serde(with = "serde_bytes", default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<[u8; 64]>,
    #[serde(default)]
    pub metadata: UserMetadata,
}

impl UserRecord {
    pub fn is_fresh(&self, now: u64, ttl: Duration) -> bool {
        now.saturating_sub(self.timestamp) <= ttl.as_secs()
    }

    pub fn validates(&self) -> bool {
        if let Some(sig_bytes) = &self.signature {
            let Ok(key) = VerifyingKey::from_bytes(&self.ipk) else {
                return false;
            };
            let msg = self.signing_payload();
            let sig = Signature::from_bytes(sig_bytes);
            return key.verify_strict(&msg, &sig).is_ok();
        }
        true // allow unsigned in early phase
    }

    fn signing_payload(&self) -> Vec<u8> {
        [
            b"user-record-v1" as &[u8],
            self.relay.as_bytes(),
            self.relay_addr.ip().to_string().as_bytes(),
            &self.relay_addr.port().to_be_bytes(),
            &self.timestamp.to_be_bytes(),
        ]
        .concat()
    }
}

/// DHT in-memory state: routing table + user presence storage.
#[derive(Debug)]
pub struct Dht {
    pub local_id: NodeId,
    pub params: DhtParams,
    routing: RoutingTable,
    users: HashMap<[u8; 32], UserRecord>,
}

impl Dht {
    pub fn new(local_id: NodeId, params: Option<DhtParams>) -> Self {
        let params = params.unwrap_or_default();
        Self {
            routing: RoutingTable::new(local_id, params.bucket_size),
            users: HashMap::new(),
            params,
            local_id,
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn upsert_node(&mut self, contact: NodeContact) {
        self.routing.insert(contact);
    }

    pub fn touch_node(&mut self, id: NodeId, addr: SocketAddr) {
        let now = Self::now_secs();
        self.routing.insert(NodeContact { id, addr, last_seen: now });
    }

    pub fn get_closest_nodes(&self, target: NodeId, count: usize) -> Vec<NodeContact> {
        self.routing.closest_nodes(target, count)
    }

    pub fn upsert_user(&mut self, record: UserRecord) -> bool {
        if !record.validates() {
            return false;
        }
        let now = Self::now_secs();
        if !record.is_fresh(now, self.params.user_ttl) {
            return false;
        }
        let key = record.ipk;
        let replaced = self.users.insert(key, record);
        replaced.is_some()
    }

    pub fn get_user(&self, ipk: &[u8; 32]) -> Option<UserRecord> {
        self.users.get(ipk).cloned()
    }

    pub fn cleanup_expired(&mut self) {
        let now = Self::now_secs();
        let ttl = self.params.user_ttl;
        self.users.retain(|_, rec| rec.is_fresh(now, ttl));
    }

    /// Pick k closest nodes to a user key for replication.
    pub fn replication_targets(&self, ipk: &[u8; 32]) -> Vec<NodeContact> {
        let target = self.derive_target_from_ipk(ipk);
        self.get_closest_nodes(target, self.params.k)
    }

    fn derive_target_from_ipk(&self, ipk: &[u8; 32]) -> NodeId {
        // Map 32-byte user key down to NodeId length using the XOR metric:
        // take closest NodeId whose bytes equal prefix of ipk.
        let mut bytes = [0u8; NodeId::LEN];
        bytes.copy_from_slice(&ipk[..NodeId::LEN]);
        NodeId::from_bytes(bytes)
    }

    pub fn closest_distance_to(&self, target: NodeId) -> Option<[u8; NodeId::LEN]> {
        self.routing
            .closest_nodes(target, 1)
            .first()
            .map(|nc| closest_distance(self.local_id, nc.id))
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn user_record_sign_verify_roundtrip() {
        use ed25519_dalek::{Signer, SigningKey};

        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let ipk = sk.verifying_key().to_bytes();
        let mut rec = UserRecord {
            ipk,
            relay: NodeId::from_bytes([1u8; NodeId::LEN]),
            relay_addr: "127.0.0.1:10000".parse().unwrap(),
            timestamp: 1,
            signature: None,
            metadata: UserMetadata::default(),
        };

        let msg = rec.signing_payload();
        let sig = sk.sign(&msg);
        rec.signature = Some(sig.to_bytes());

        assert!(rec.validates());
    }
}
