use std::cmp::Ordering;
use std::net::SocketAddr;

use common::quic::id::NodeId;

use crate::dht::NodeContact;

/// XOR distance between two NodeIds (big-endian byte array).
pub fn xor_distance(a: NodeId, b: NodeId) -> [u8; NodeId::LEN] {
    let mut out = [0u8; NodeId::LEN];
    for (i, (x, y)) in a.as_bytes().iter().zip(b.as_bytes()).enumerate() {
        out[i] = x ^ y;
    }
    out
}

pub fn closest_distance(a: NodeId, b: NodeId) -> [u8; NodeId::LEN] {
    xor_distance(a, b)
}

fn leading_zero_bits(xor: &[u8; NodeId::LEN]) -> usize {
    let mut idx = 0;
    for byte in xor {
        if *byte == 0 {
            idx += 8;
            continue;
        }
        idx += byte.leading_zeros() as usize;
        break;
    }
    idx
}

#[derive(Debug, Clone)]
struct Bucket {
    entries: Vec<NodeContact>,
    max: usize,
}

impl Bucket {
    fn new(max: usize) -> Self {
        Self { entries: Vec::with_capacity(max), max }
    }

    fn insert(&mut self, contact: NodeContact) {
        if let Some(existing) = self.entries.iter_mut().find(|c| c.id == contact.id) {
            *existing = contact;
            return;
        }

        if self.entries.len() < self.max {
            self.entries.push(contact);
        } else if let Some(oldest_idx) =
            self.entries.iter().enumerate().min_by_key(|(_, c)| c.last_seen).map(|(i, _)| i)
        {
            self.entries[oldest_idx] = contact;
        }
    }

    fn nodes(&self) -> impl Iterator<Item = &NodeContact> {
        self.entries.iter()
    }
}

#[derive(Debug)]
pub struct RoutingTable {
    local_id: NodeId,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    pub fn new(local_id: NodeId, bucket_size: usize) -> Self {
        // number of buckets = bit-length of NodeId
        let num_buckets = NodeId::LEN * 8;
        let buckets = (0..num_buckets).map(|_| Bucket::new(bucket_size)).collect();

        Self { local_id, buckets }
    }

    fn bucket_index(&self, id: NodeId) -> usize {
        if id == self.local_id {
            return self.buckets.len() - 1;
        }
        let xor = xor_distance(self.local_id, id);
        let lz = leading_zero_bits(&xor);
        lz.min(self.buckets.len() - 1)
    }

    pub fn insert(&mut self, contact: NodeContact) {
        let idx = self.bucket_index(contact.id);
        self.buckets[idx].insert(contact);
    }

    pub fn closest_nodes(&self, target: NodeId, count: usize) -> Vec<NodeContact> {
        let mut nodes: Vec<NodeContact> = self
            .buckets
            .iter()
            .flat_map(|b| b.nodes().cloned())
            .collect();

        nodes.sort_by(|a, b| compare_distance(target, a.id, b.id));
        nodes.truncate(count);
        nodes
    }
}

fn compare_distance(target: NodeId, a: NodeId, b: NodeId) -> Ordering {
    let da = xor_distance(target, a);
    let db = xor_distance(target, b);
    da.cmp(&db)
}

impl From<&RoutingTable> for Vec<SocketAddr> {
    fn from(rt: &RoutingTable) -> Self {
        rt.buckets
            .iter()
            .flat_map(|b| b.entries.iter().map(|c| c.addr))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn node(id_byte: u8) -> NodeId {
        NodeId::from_bytes([id_byte; NodeId::LEN])
    }

    fn contact(id: NodeId) -> NodeContact {
        NodeContact {
            id,
            addr: "127.0.0.1:5000".parse().unwrap(),
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    #[test]
    fn insert_and_get_closest() {
        let local = node(0x00);
        let mut rt = RoutingTable::new(local, 4);

        for b in 1..10 {
            rt.insert(contact(node(b)));
        }

        let closest = rt.closest_nodes(node(0x02), 3);
        assert!(!closest.is_empty());
        assert!(closest.len() <= 3);
    }

    #[test]
    fn replaces_oldest_when_full() {
        let local = node(0x00);
        let mut rt = RoutingTable::new(local, 2);

        let mut first = contact(node(1));
        first.last_seen = 1;
        let mut second = contact(node(2));
        second.last_seen = 2;
        let mut third = contact(node(3));
        third.last_seen = 3;

        rt.insert(first.clone());
        rt.insert(second.clone());
        rt.insert(third.clone());

        let nodes = rt.closest_nodes(node(2), 10);
        assert!(nodes.iter().any(|c| c.id == third.id));
    }
}
