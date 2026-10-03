use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use common::info;
use common::proto::RelayId;
use common::proto::client_res::RelayDescriptor;
use common::quic::CloseReason;
use common::server::accept::source_group;
use common::types::bytes::Bytes;
use common::warn;
use parking_lot::Mutex;
use parking_lot::RwLock;
use quinn::Connection;
use tokio::task::JoinHandle;

/// Counted per [`source_group`]. A relay's registration proves only possession of a fresh keypair,
/// so the source address is the one scarce resource an unauthenticated peer has to spend.
pub const MAX_REGISTRATIONS_PER_SOURCE: usize = 8;

#[derive(Debug, Clone)]
pub struct RelayEntry {
    pub id: RelayId,
    pub conn: Arc<Connection>,
    pub pubkey: Bytes<32>,
    pub last_heartbeat_at: Arc<Mutex<Instant>>,
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

    pub fn last_heartbeat_at(&self) -> Instant {
        *self.last_heartbeat_at.lock()
    }

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

/// The registered relays or gateways, keyed by id; the last connection to register an id wins.
#[derive(Debug)]
pub struct Directory {
    pub kind: &'static str,
    map: RwLock<HashMap<RelayId, RelayEntry>>,
    cap: usize,
    /// Gateways send no heartbeat, so eviction would always take a live one.
    evict: bool,
    /// Bumped on every membership change; invalidates the cached `GetRelays` response.
    pub generation: AtomicU64,
}

impl Directory {
    pub fn new(kind: &'static str, cap: usize, evict: bool) -> Arc<Self> {
        Arc::new(Self { kind, map: RwLock::default(), cap, evict, generation: AtomicU64::new(0) })
    }

    pub fn admit(
        &self, id: RelayId, pubkey: Bytes<32>, conn: Arc<Connection>,
    ) -> Result<(), CloseReason> {
        let kind = self.kind;
        let mut map = self.map.write();

        // The pointer-guarded watcher (`watch`) makes the displaced connection's cleanup a no-op,
        // so it cannot evict the new entry.
        let replaced = map.remove(&id);
        if let Some(existing) = &replaced
            && !Arc::ptr_eq(&existing.conn, &conn)
            && existing.conn.close_reason().is_none()
        {
            info!("{kind}({id}) reconnected, superseding prior session");
            CloseReason::Reconnecting.close(&existing.conn);
        }

        let slots: Vec<Slot> = map.values().map(RelayEntry::slot).collect();
        match admit(&slots, conn.remote_address().ip(), self.cap, Instant::now()) {
            Admission::Insert => {},
            Admission::Evict(victim) if self.evict => {
                if let Some(evicted) = map.remove(&victim) {
                    CloseReason::RegistryFull.close(&evicted.conn);
                }
            },
            _ => {
                if replaced.is_some() {
                    self.generation.fetch_add(1, Ordering::Release);
                }
                warn!(
                    "{kind}({}) rejected: no admissible slot ({}/{})",
                    conn.remote_address(),
                    map.len(),
                    self.cap
                );
                return Err(CloseReason::RegistryFull);
            },
        }

        map.insert(id, RelayEntry::new(id, conn, pubkey));
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Removes the entry once its connection closes, but only while the entry still holds that
    /// connection: a re-registration can land between `closed()` and the write lock.
    pub fn watch(self: &Arc<Self>, id: RelayId, conn: Arc<Connection>) -> JoinHandle<()> {
        let dir = self.clone();
        tokio::spawn(async move {
            let _ = conn.closed().await;
            let mut map = dir.map.write();
            if map.get(&id).is_some_and(|e| Arc::ptr_eq(&e.conn, &conn)) {
                map.remove(&id);
                dir.generation.fetch_add(1, Ordering::Release);
            }
        })
    }

    /// The entry must hold this very connection, so a signed heartbeat replayed over another
    /// session cannot refresh its liveness.
    pub fn touch(&self, id: &RelayId, conn: &Arc<Connection>) -> bool {
        let map = self.map.read();
        let entry = map.get(id).filter(|e| Arc::ptr_eq(&e.conn, conn));
        entry.inspect(|e| e.touch_heartbeat(Instant::now())).is_some()
    }

    pub fn snapshot(&self) -> Vec<RelayEntry> {
        self.map.read().values().cloned().collect()
    }

    pub fn descriptors(&self) -> Vec<RelayDescriptor> {
        self.map.read().values().map(RelayEntry::to_descriptor).collect()
    }

    pub fn close_all(&self) {
        for entry in self.map.read().values() {
            entry.conn.close(CloseReason::ShuttingDown.code(), b"ResolverShuttingDown");
        }
    }
}

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

/// Three intervals, so two lost heartbeats do not cost a relay its eviction protection.
pub const HEARTBEAT_TIMEOUT: Duration =
    Duration::from_secs(common::quic::RESOLVER_RELAY_HEARTBEAT_INTERVAL * 3);

/// `slots` must exclude the entry being replaced. At capacity, only a slot that never heartbeated
/// or went silent past [`HEARTBEAT_TIMEOUT`] is evicted: a flood cannot push out a live relay.
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
    use std::net::Ipv6Addr;

    use super::*;

    fn id(seed: u8) -> RelayId {
        RelayId::from_bytes([seed; 32])
    }

    fn slot(seed: u8, ip: IpAddr, established: bool, last_seen: Instant) -> Slot {
        Slot { id: id(seed), ip, established, last_seen }
    }

    #[test]
    fn admission_caps_each_source_and_evicts_only_silent_relays() {
        let t0 = Instant::now();
        let (ms, second) = (Duration::from_millis(1), Duration::from_secs(1));
        let v4 = |last| IpAddr::V4(Ipv4Addr::new(10, 0, 0, last));
        let v6 = |subnet, host| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, subnet, 0, 0, 0, host));
        let full = |ip: &dyn Fn(u8) -> IpAddr| -> Vec<Slot> {
            (0..MAX_REGISTRATIONS_PER_SOURCE as u8).map(|i| slot(i, ip(i), false, t0)).collect()
        };
        let one_v4 = full(&|_| v4(9));
        let one_64 = full(&|i| v6(1, u16::from(i)));
        let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 9).to_ipv6_mapped());
        let mixed = [
            slot(1, v4(1), true, t0),
            slot(2, v4(2), false, t0 + 2 * second),
            slot(3, v4(3), false, t0 + second),
        ];
        let live = [slot(1, v4(1), true, t0), slot(2, v4(2), true, t0 + second)];

        use Admission::*;
        let cases: [(&[Slot], IpAddr, usize, Instant, Admission); 9] = [
            (&one_v4, v4(9), 1024, t0, Reject),
            (&one_v4, mapped, 1024, t0, Reject),
            (&one_v4, v4(8), 1024, t0, Insert),
            (&one_64, v6(1, 999), 1024, t0, Reject),
            (&one_64, v6(2, 1), 1024, t0, Insert),
            (&one_v4, v4(9), one_v4.len(), t0, Reject),
            (&mixed, v4(4), mixed.len(), t0 + 2 * second, Evict(id(3))),
            (&live, v4(3), live.len(), t0 + HEARTBEAT_TIMEOUT - ms, Reject),
            (&live, v4(3), live.len(), t0 + HEARTBEAT_TIMEOUT, Evict(id(1))),
        ];
        for (i, (slots, ip, capacity, now, want)) in cases.into_iter().enumerate() {
            assert_eq!(admit(slots, ip, capacity, now), want, "case {i}");
        }
    }
}
