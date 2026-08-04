use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use common::proto::RelayId;
use common::proto::client_res::RelayDescriptor;
use common::types::bytes::Bytes;
use parking_lot::Mutex;
use quinn::Connection;

/// Registrations one [`source_group`] may hold at once. Registration proves
/// only possession of a freshly generated keypair, so the source address is
/// the one scarce resource an unauthenticated peer has to spend.
pub const MAX_REGISTRATIONS_PER_SOURCE: usize = 8;

/// Per-relay registry entry held under the resolver's `relays` map.
///
/// `last_heartbeat_at` is the resolver's local-clock observation of the
/// most recent authenticated `RelayHello`/`RelayHeartbeat` from this
/// relay. It is used as the recency proxy for the `rtt_near` ranking in
/// [`ClientRequest::GetBootstrapPeers`]: until the resolver tracks
/// per-relay RTT directly (Vivaldi-or-similar, future work),
/// most-recently-heard-from is the best signal of "this relay has
/// good network position towards us."
///
/// Stored as `Instant` rather than ms-since-epoch so the recency
/// comparison is monotonic regardless of wall-clock jumps. Wrapped in a
/// `Mutex` so the heartbeat path can update it under the registry's
/// outer `RwLock` *read* guard — a recency bump is per-entry-local state
/// that doesn't need to gate every other reader on the map.
///
/// [`ClientRequest::GetBootstrapPeers`]: common::proto::client_res::ClientRequest::GetBootstrapPeers
#[derive(Debug, Clone)]
pub struct RelayEntry {
    pub id: RelayId,
    pub conn: Arc<Connection>,
    /// Relay's full Ed25519 identity public key, captured from the
    /// authenticated `RelayHello` at registration time. Carried so the
    /// resolver can include it in [`RelayDescriptor`] responses without
    /// re-deriving from the cert chain on every `GetRelays` /
    /// `GetBootstrapPeers` call. See `RelayDescriptor::pubkey` doc for
    /// why bootstrap consumers need this.
    pub pubkey: Bytes<32>,
    /// Instant of the last authenticated lifetime packet
    /// (`RelayHello` or `RelayHeartbeat`). Wrapped in `Arc<Mutex<...>>`
    /// so heartbeat-driven updates don't require the outer registry
    /// `RwLock` to be taken in write mode.
    pub last_heartbeat_at: Arc<Mutex<Instant>>,
    /// Set by the first authenticated heartbeat. An established entry is
    /// one whose holder stayed connected past a heartbeat interval, which
    /// is what [`admit`] refuses to evict for a newcomer.
    established: Arc<AtomicBool>,
}

impl RelayEntry {
    pub fn new(id: RelayId, conn: Arc<Connection>, pubkey: Bytes<32>) -> Self {
        Self {
            id,
            conn,
            pubkey,
            last_heartbeat_at: Arc::new(Mutex::new(Instant::now())),
            established: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn to_descriptor(&self) -> RelayDescriptor {
        descriptor(self.id, self.conn.remote_address(), self.pubkey)
    }

    /// Latest observation of this relay's liveness, as an [`Instant`].
    /// Cloned out of the per-entry `Mutex` so callers don't hold the
    /// lock across whatever they do next.
    pub fn last_heartbeat_at(&self) -> Instant {
        *self.last_heartbeat_at.lock()
    }

    /// Update [`Self::last_heartbeat_at`] to `now`. Called from the
    /// authenticated `RelayHeartbeat` path. The update is unconditional
    /// — the caller has already verified the heartbeat is fresh and
    /// well-signed (`Resolver::verify_heartbeat`), so an out-of-order
    /// arrival should still bump recency: it's a strictly newer
    /// observation than whatever was stored before.
    pub fn touch_heartbeat(&self, now: Instant) {
        *self.last_heartbeat_at.lock() = now;
        self.established.store(true, Ordering::Relaxed);
    }

    pub fn slot(&self) -> Slot {
        Slot {
            id:          self.id,
            ip:          self.conn.remote_address().ip(),
            established: self.established.load(Ordering::Relaxed),
            last_seen:   self.last_heartbeat_at(),
        }
    }
}

/// A descriptor's address is the resolver's own observation of the peer;
/// nothing on the wire can influence it.
fn descriptor(id: RelayId, addr: SocketAddr, pubkey: Bytes<32>) -> RelayDescriptor {
    RelayDescriptor { id, addr, pubkey }
}

/// An occupied registry slot reduced to the fields [`admit`] ranks on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub id:          RelayId,
    pub ip:          IpAddr,
    pub established: bool,
    pub last_seen:   Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Insert,
    Evict(RelayId),
    Reject,
}

/// Quota key for a source address. IPv4 counts per address; IPv6 counts per
/// /64, because a single host is routinely handed a whole /64 and could
/// otherwise present an unlimited supply of distinct addresses.
fn source_group(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0).octets()
        },
    }
}

/// How long an established slot keeps its eviction protection without a
/// heartbeat. Three intervals tolerates two lost heartbeats before a relay is
/// treated as gone.
pub const HEARTBEAT_TIMEOUT: Duration =
    Duration::from_secs(common::quic::RESOLVER_RELAY_HEARTBEAT_INTERVAL * 3);

/// Decide whether a registration from `applicant_ip` may take a slot.
///
/// `slots` must already exclude any entry the applicant is replacing under
/// last-connection-wins. At capacity a slot is displaced only if it has yet to
/// heartbeat or has gone silent past [`HEARTBEAT_TIMEOUT`], least-recently
/// -heard-from first: a flood of fresh identities cannot push out a live relay,
/// and a squatter that heartbeats once cannot hold a slot forever.
pub fn admit(
    slots: &[Slot], applicant_ip: IpAddr, capacity: usize, now: Instant,
) -> Admission {
    let group = source_group(applicant_ip);
    let from_group = slots.iter().filter(|s| source_group(s.ip) == group).count();
    if from_group >= MAX_REGISTRATIONS_PER_SOURCE {
        return Admission::Reject;
    }

    if slots.len() < capacity {
        return Admission::Insert;
    }

    slots
        .iter()
        .filter(|s| !s.established || now.duration_since(s.last_seen) >= HEARTBEAT_TIMEOUT)
        .min_by_key(|s| s.last_seen)
        .map_or(Admission::Reject, |s| Admission::Evict(s.id))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;

    fn id(seed: u8) -> RelayId {
        let mut b = [0u8; 32];
        b[0] = seed;
        RelayId::from_bytes(b)
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    fn slot(seed: u8, ip: IpAddr, established: bool, last_seen: Instant) -> Slot {
        Slot { id: id(seed), ip, established, last_seen }
    }

    #[test]
    fn admits_while_under_capacity() {
        let now = Instant::now();
        let slots = [slot(1, ip(1), true, now)];
        assert_eq!(admit(&slots, ip(2), 8, Instant::now()), Admission::Insert);
    }

    #[test]
    fn rejects_past_the_per_source_cap() {
        let now = Instant::now();
        let slots: Vec<Slot> = (0..MAX_REGISTRATIONS_PER_SOURCE as u8)
            .map(|i| slot(i, ip(9), false, now))
            .collect();
        assert_eq!(admit(&slots, ip(9), 1024, Instant::now()), Admission::Reject);
        assert_eq!(admit(&slots, ip(8), 1024, Instant::now()), Admission::Insert);
    }

    #[test]
    fn per_source_cap_counts_only_the_applicant_group() {
        let now = Instant::now();
        let mut slots: Vec<Slot> = (0..MAX_REGISTRATIONS_PER_SOURCE as u8)
            .map(|i| slot(i, ip(9), false, now))
            .collect();
        slots.push(slot(100, ip(8), false, now));
        assert_eq!(admit(&slots, ip(8), 1024, Instant::now()), Admission::Insert);
    }

    #[test]
    fn ipv6_addresses_share_a_quota_across_the_same_64() {
        let now = Instant::now();
        let v6 = |host: u16| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, host));
        let slots: Vec<Slot> = (0..MAX_REGISTRATIONS_PER_SOURCE as u8)
            .map(|i| slot(i, v6(i as u16), false, now))
            .collect();

        assert_eq!(admit(&slots, v6(999), 1024, Instant::now()), Admission::Reject);

        let other_64 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1));
        assert_eq!(admit(&slots, other_64, 1024, Instant::now()), Admission::Insert);
    }

    #[test]
    fn at_capacity_evicts_the_oldest_unestablished_slot() {
        let now = Instant::now();
        let slots = [
            slot(1, ip(1), true, now),
            slot(2, ip(2), false, now - Duration::from_secs(30)),
            slot(3, ip(3), false, now - Duration::from_secs(120)),
        ];
        assert_eq!(admit(&slots, ip(4), slots.len(), now), Admission::Evict(id(3)));
    }

    #[test]
    fn at_capacity_never_evicts_a_live_established_slot() {
        let now = Instant::now();
        let slots = [slot(1, ip(1), true, now), slot(2, ip(2), true, now)];
        assert_eq!(admit(&slots, ip(3), slots.len(), now), Admission::Reject);
    }

    #[test]
    fn an_established_slot_that_stopped_heartbeating_ages_out() {
        let now = Instant::now();
        let slots = [slot(1, ip(1), true, now - HEARTBEAT_TIMEOUT), slot(2, ip(2), true, now)];
        assert_eq!(admit(&slots, ip(3), slots.len(), now), Admission::Evict(id(1)));
    }

    #[test]
    fn descriptor_carries_the_observed_socket_addr() {
        let addr = SocketAddr::from(([203, 0, 113, 7], 4433));
        let d = descriptor(id(5), addr, Bytes([7u8; 32]));
        assert_eq!(d.addr, addr);
        assert_eq!(d.id, id(5));
        assert_eq!(d.pubkey, Bytes([7u8; 32]));
    }
}
