//! Kademlia routing table over the shared `NodeId`/IPK keyspace.

use std::net::SocketAddr;
use std::time::Instant;

use common::proto::dht_p2p::NodeDescriptor;
use common::quic::id::NodeId;
use common::quic::xor32;

use super::Dht;
use super::config::BUCKETS;
use super::config::BUCKET_SIZE;
use super::config::K;

pub(crate) const PING_FAILURES_BEFORE_EVICTION: u8 = 3;

#[derive(Debug, Clone)]
pub struct RoutingEntry {
    pub id: NodeId,

    pub addr: SocketAddr,

    /// Hashes to `id`: every caller of [`RoutingTable::insert`] checks that binding.
    pub pubkey: [u8; 32],

    pub last_seen: Instant,

    pub failed_pings: u8,
}

impl RoutingEntry {
    pub(crate) fn descriptor(&self) -> NodeDescriptor {
        NodeDescriptor {
            id:     self.id,
            addr:   self.addr,
            pubkey: self.pubkey.into(),
        }
    }

    pub(crate) fn from_descriptor(desc: &NodeDescriptor) -> Self {
        Self {
            id:           desc.id,
            addr:         desc.addr,
            pubkey:       desc.pubkey.0,
            last_seen:    Instant::now(),
            failed_pings: 0,
        }
    }
}

/// `entries` runs from least to most recently seen. Both vectors stay within `BUCKET_SIZE`.
#[derive(Debug)]
pub struct Bucket {
    pub entries: Vec<RoutingEntry>,

    pub refresh_at: Instant,

    /// Replacement cache, promoted when an entry is evicted. The cap keeps an attacker spraying
    /// descriptors at a full bucket from growing memory.
    pub candidates: Vec<RoutingEntry>,
}

impl Bucket {
    pub(crate) fn empty(now: Instant) -> Self {
        Self {
            entries:    Vec::with_capacity(BUCKET_SIZE),
            refresh_at: now,
            candidates: Vec::with_capacity(BUCKET_SIZE),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    IsSelf,

    Refreshed,

    Inserted,

    /// The bucket is full and the newcomer is parked; the caller must probe the returned LRU entry.
    PendingPing(NodeDescriptor),

    /// Already parked, or bucket and cache are both full. Old candidates are never displaced, so an
    /// attacker cannot flush good ones.
    Discarded,
}

#[derive(Debug)]
pub struct RoutingTable {
    pub self_id: NodeId,

    /// `buckets[i]` holds peers whose XOR distance has `BUCKETS - 1 - i` leading zero bits.
    pub buckets: Box<[Bucket; BUCKETS]>,
}

impl RoutingTable {
    pub fn empty(self_id: NodeId) -> Self {
        let now = Instant::now();
        let buckets: Vec<Bucket> = (0..BUCKETS).map(|_| Bucket::empty(now)).collect();
        let buckets: Box<[Bucket; BUCKETS]> = buckets
            .into_boxed_slice()
            .try_into()
            .expect("BUCKETS-sized vec converts to BUCKETS-sized array");
        Self { self_id, buckets }
    }

    pub fn insert(&mut self, descriptor: NodeDescriptor) -> InsertOutcome {
        let Some(bucket_idx) = bucket_for(&self.self_id, &descriptor.id) else {
            return InsertOutcome::IsSelf;
        };
        let now = Instant::now();
        let bucket = &mut self.buckets[bucket_idx];
        bucket.refresh_at = now;

        if let Some(pos) = bucket.entries.iter().position(|e| e.id == descriptor.id) {
            let mut entry = bucket.entries.remove(pos);
            entry.last_seen = now;
            entry.addr = descriptor.addr;
            // Only a pubkey that hashes to the id can be the real one, so a self-consistent
            // descriptor wins over a cached pubkey that never proved its binding.
            if NodeId::new(descriptor.pubkey.0) == descriptor.id {
                entry.pubkey = descriptor.pubkey.0;
            }
            bucket.entries.push(entry);
            return InsertOutcome::Refreshed;
        }

        if bucket.entries.len() < BUCKET_SIZE {
            bucket.entries.push(RoutingEntry::from_descriptor(&descriptor));
            return InsertOutcome::Inserted;
        }

        if bucket.candidates.len() < BUCKET_SIZE {
            if bucket.candidates.iter().any(|c| c.id == descriptor.id) {
                return InsertOutcome::Discarded;
            }
            let lru_descriptor = bucket.entries[0].descriptor();
            bucket.candidates.push(RoutingEntry::from_descriptor(&descriptor));
            return InsertOutcome::PendingPing(lru_descriptor);
        }

        InsertOutcome::Discarded
    }

    pub(crate) fn ping_failed(&mut self, peer_id: &NodeId) {
        let Some(bucket_idx) = bucket_for(&self.self_id, peer_id) else {
            return;
        };
        let bucket = &mut self.buckets[bucket_idx];
        let Some(pos) = bucket.entries.iter().position(|e| e.id == *peer_id) else {
            return;
        };

        let entry = &mut bucket.entries[pos];
        entry.failed_pings = entry.failed_pings.saturating_add(1);
        if entry.failed_pings < PING_FAILURES_BEFORE_EVICTION {
            return;
        }

        bucket.entries.remove(pos);

        if !bucket.candidates.is_empty() {
            let mut promoted = bucket.candidates.remove(0);
            promoted.last_seen = Instant::now();
            promoted.failed_pings = 0;
            bucket.entries.push(promoted);
        }
    }

    pub(crate) fn ping_succeeded(&mut self, peer_id: &NodeId) -> bool {
        let Some(bucket_idx) = bucket_for(&self.self_id, peer_id) else {
            return false;
        };
        let bucket = &mut self.buckets[bucket_idx];
        let Some(pos) = bucket.entries.iter().position(|e| e.id == *peer_id) else {
            return false;
        };

        let now = Instant::now();
        let entry = &mut bucket.entries[pos];
        entry.failed_pings = 0;
        entry.last_seen = now;

        let entry = bucket.entries.remove(pos);
        bucket.entries.push(entry);
        bucket.refresh_at = now;
        true
    }

    pub(crate) fn get(&self, id: &NodeId) -> Option<&RoutingEntry> {
        self.buckets[bucket_for(&self.self_id, id)?].entries.iter().find(|e| e.id == *id)
    }

    pub(crate) fn find_closest(&self, target: &NodeId, count: usize) -> Vec<NodeDescriptor> {
        self.closest(target, count).into_iter().map(|e| e.descriptor()).collect()
    }

    pub(crate) fn closest(&self, target: &NodeId, count: usize) -> Vec<RoutingEntry> {
        let mut scratch: Vec<([u8; 32], &RoutingEntry)> = self
            .buckets
            .iter()
            .flat_map(|bucket| &bucket.entries)
            .map(|entry| (xor32(entry.id.as_bytes(), target.as_bytes()), entry))
            .collect();
        if count < scratch.len() {
            scratch.select_nth_unstable_by_key(count, |(dist, _)| *dist);
            scratch.truncate(count);
        }
        scratch.sort_unstable_by_key(|(dist, _)| *dist);
        scratch.into_iter().map(|(_, e)| e.clone()).collect()
    }

    pub(crate) fn total_known(&self) -> usize {
        self.buckets.iter().map(|b| b.entries.len()).sum()
    }

    /// Skips empty buckets, which would make refresh traffic scale with the keyspace instead of the
    /// table. Leaves `refresh_at` alone, so a failed walk is retried.
    pub(crate) fn buckets_needing_refresh(&self, now: Instant) -> Vec<usize> {
        let threshold = std::time::Duration::from_millis(super::config::BUCKET_REFRESH_MS);
        let mut out = Vec::new();
        for (idx, bucket) in self.buckets.iter().enumerate() {
            if bucket.entries.is_empty() {
                continue;
            }
            if let Some(age) = now.checked_duration_since(bucket.refresh_at)
                && age >= threshold {
                    out.push(idx);
                }
        }
        out
    }

    pub(crate) fn mark_refreshed(&mut self, bucket_idx: usize) {
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            bucket.refresh_at = Instant::now();
        }
    }
}

pub(crate) fn bucket_for(self_id: &NodeId, peer_id: &NodeId) -> Option<usize> {
    let xor = xor32(self_id.as_bytes(), peer_id.as_bytes());

    let mut lzc: usize = 0;
    for &b in xor.iter() {
        if b == 0 {
            lzc += 8;
        } else {
            lzc += b.leading_zeros() as usize;
            break;
        }
    }

    if lzc == 256 {
        None
    } else {
        Some(BUCKETS - 1 - lzc)
    }
}

pub(crate) fn random_id_in_bucket(self_id: &NodeId, bucket_idx: usize) -> Option<NodeId> {
    use rand::TryRng;
    use rand::rngs::SysRng;

    if bucket_idx >= BUCKETS {
        return None;
    }
    let mut noise = [0u8; 32];
    SysRng.try_fill_bytes(&mut noise).ok()?;

    // `bucket_for` is `255 - leading_zeros(self ^ t)`, so t must agree
    // with self above `first_diff` and differ at it; bits below are free.
    let first_diff = BUCKETS - 1 - bucket_idx;
    let mut out = *self_id.as_bytes();
    out[first_diff / 8] ^= 0x80 >> (first_diff % 8);
    for bit in (first_diff + 1)..BUCKETS {
        let mask = 0x80u8 >> (bit % 8);
        out[bit / 8] = (out[bit / 8] & !mask) | (noise[bit / 8] & mask);
    }
    Some(NodeId::from_bytes(out))
}

/// The remote homes of `target`, and whether this relay is one.
/// With fewer than `K` known peers, this relay and every known peer are homes.
pub(crate) fn homes(dht: &Dht, target: &NodeId) -> (Vec<NodeDescriptor>, bool) {
    let mut remote = dht.routing.read().find_closest(target, K);
    let is_home = remote.len() < K
        || xor32(dht.node_id.as_bytes(), target.as_bytes())
            <= xor32(remote[K - 1].id.as_bytes(), target.as_bytes());
    if is_home {
        remote.truncate(K - 1);
    }
    (remote, is_home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: NodeId) -> NodeDescriptor {
        NodeDescriptor { id, addr: "127.0.0.1:1".parse().unwrap(), pubkey: [0; 32].into() }
    }

    fn hashed(n: u32) -> NodeId {
        NodeId::new(n.to_be_bytes())
    }

    /// Every relay must choose the same homes, so the table answers in exact XOR order.
    #[test]
    fn find_closest_is_the_xor_order_of_every_known_peer() {
        let mut table = RoutingTable::empty(hashed(0));
        let known: Vec<NodeId> = (1..=40)
            .map(hashed)
            .filter(|id| table.insert(peer(*id)) == InsertOutcome::Inserted)
            .collect();
        for target in (100..110).map(hashed) {
            let mut expected = known.clone();
            expected.sort_by_key(|id| xor32(id.as_bytes(), target.as_bytes()));
            let found: Vec<NodeId> = table.find_closest(&target, K).iter().map(|d| d.id).collect();
            assert_eq!(found, expected[..K]);
        }
    }

    #[test]
    fn a_refresh_target_lands_in_the_bucket_it_was_drawn_for() {
        let me = hashed(3);
        for bucket in 0..BUCKETS {
            let target = random_id_in_bucket(&me, bucket).unwrap();
            assert_eq!(bucket_for(&me, &target), Some(bucket));
        }
        assert!(random_id_in_bucket(&me, BUCKETS).is_none());
    }

    /// One relay is a whole network: with fewer than `K` peers known, this relay and every peer
    /// are homes. Rows: peers known, then (remote homes, self a home) for a target near the
    /// peers and for one near this relay.
    #[test]
    fn this_relay_is_a_home_until_k_closer_peers_are_known() {
        let (_dir, dht) = crate::test_support::dht(NodeId::from_bytes([0xFF; 32]));
        let near_peers = NodeId::from_bytes([0; 32]);
        let near_me = NodeId::from_bytes([0xFE; 32]);
        let homes_of = |target| {
            let (remote, is_home) = homes(&dht, &target);
            (remote.len(), is_home)
        };
        let mut rows = Vec::new();
        for known in 0..=4u8 {
            if known > 0 {
                let mut id = [0; 32];
                id[31] = known;
                dht.routing.write().insert(peer(NodeId::from_bytes(id)));
            }
            rows.push((known, homes_of(near_peers), homes_of(near_me)));
        }
        assert_eq!(
            rows,
            [
                (0, (0, true), (0, true)),
                (1, (1, true), (1, true)),
                (2, (2, true), (2, true)),
                (3, (3, false), (2, true)),
                (4, (3, false), (2, true)),
            ]
        );
    }

    /// Eclipse resistance: a full bucket parks newcomers, keeps its parked candidates, and
    /// replaces its oldest entry only after three failed probes in a row.
    #[test]
    fn a_full_bucket_replaces_an_entry_only_after_three_failed_probes_in_a_row() {
        let mut table = RoutingTable::empty(NodeId::from_bytes([0; 32]));
        let far = |n: u8| {
            let mut id = [0x80; 32];
            id[31] = n;
            NodeId::from_bytes(id)
        };
        for n in 0..BUCKET_SIZE as u8 {
            assert_eq!(table.insert(peer(far(n))), InsertOutcome::Inserted);
        }
        assert_eq!(table.insert(peer(far(100))), InsertOutcome::PendingPing(peer(far(0))));
        for n in 101..100 + BUCKET_SIZE as u8 {
            assert!(matches!(table.insert(peer(far(n))), InsertOutcome::PendingPing(_)));
        }
        assert_eq!(table.insert(peer(far(200))), InsertOutcome::Discarded);

        let oldest = far(0);
        table.ping_failed(&oldest);
        table.ping_failed(&oldest);
        assert!(table.ping_succeeded(&oldest));
        table.ping_failed(&oldest);
        table.ping_failed(&oldest);
        assert!(table.get(&oldest).is_some(), "an answer resets the count");
        table.ping_failed(&oldest);
        assert!(table.get(&oldest).is_none());
        assert!(table.get(&far(100)).is_some(), "the first parked candidate takes the slot");
    }
}
