//! Authenticated attachment bridges over the TLS fallback tunnel, kept apart from the UDP
//! bearer-token bridges so that knowing a token cannot claim an endpoint of another identity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use common::quic::tunnel::{AcceptedMode, Channel};
use parking_lot::Mutex;

const MAX_BRIDGES: usize = 512;
const MAX_BRIDGES_PER_IDENTITY: usize = 8;
// 8 MiB/s sustained per participant; enough for a file transfer while
// keeping a cheap authenticated identity from monopolising relay egress.
const BYTES_PER_SECOND: f64 = 8.0 * 1024.0 * 1024.0;
const BYTE_BURST: f64 = 256.0 * 1024.0;

#[derive(Default)]
pub struct Assist {
    routes: Arc<Mutex<Registry<Arc<Channel>>>>,
}

impl Assist {
    pub async fn serve(&self, channel: Arc<Channel>, mode: AcceptedMode) {
        let AcceptedMode::Assist { token, ipk, peer } = mode else {
            channel.close();
            return;
        };
        let registered = self.routes.lock().register(token, ipk, peer, channel.clone());
        let Ok((registration, previous)) = registered else {
            channel.close();
            return;
        };
        // Close outside the registry lock. The displaced task's guard cannot
        // remove this newer registration when its read loop wakes up.
        if let Some(previous) = previous {
            previous.close();
        }
        let _lease = Lease { routes: self.routes.clone(), registration };
        let mut tokens = BYTE_BURST;
        let mut updated = Instant::now();
        while let Ok(packet) = channel.recv().await {
            let now = Instant::now();
            tokens = (tokens + now.duration_since(updated).as_secs_f64() * BYTES_PER_SECOND)
                .min(BYTE_BURST);
            updated = now;
            if tokens < packet.len() as f64 {
                // Datagram loss is recoverable by the opaque QUIC layer.
                // Never queue an unbounded bandwidth debt.
                continue;
            }
            tokens -= packet.len() as f64;
            let routes = self.routes.lock();
            if !routes.current(&registration) {
                break;
            }
            if let Some(destination) = routes.destination(&registration) {
                // Enqueueing without blocking under the generation lock makes replacement and
                // forwarding linearizable; a full queue drops the datagram, so no route waits.
                let _ = destination.try_send(&packet);
            }
        }
        channel.close();
    }
}

struct Lease {
    routes: Arc<Mutex<Registry<Arc<Channel>>>>,
    registration: Registration,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.routes.lock().remove(&self.registration);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Registration {
    token: [u8; 16],
    slot: usize,
    generation: u64,
}

struct Participant<T> {
    generation: u64,
    value: T,
}
struct Bridge<T> {
    identities: [[u8; 32]; 2],
    participants: [Option<Participant<T>>; 2],
}
struct Registry<T> {
    bridges: HashMap<[u8; 16], Bridge<T>>,
    generation: u64,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self { bridges: HashMap::new(), generation: 0 }
    }
}

impl<T> Registry<T> {
    fn register(
        &mut self, token: [u8; 16], ipk: [u8; 32], peer: [u8; 32], value: T,
    ) -> Result<(Registration, Option<T>), ()> {
        if ipk == peer {
            return Err(());
        }
        let identities = if ipk < peer { [ipk, peer] } else { [peer, ipk] };
        let slot = usize::from(ipk > peer);
        if let Some(bridge) = self.bridges.get(&token) {
            if bridge.identities != identities {
                return Err(());
            }
        } else {
            if self.bridges.len() >= MAX_BRIDGES {
                return Err(());
            }
        }
        // Count live participants, not targets someone else named, so an attacker cannot spend
        // another IPK's quota by naming it as its remote.
        let replacing =
            self.bridges.get(&token).is_some_and(|bridge| bridge.participants[slot].is_some());
        if !replacing {
            let owned = self
                .bridges
                .values()
                .filter(|bridge| {
                    bridge.identities.iter().enumerate().any(|(index, identity)| {
                        *identity == ipk && bridge.participants[index].is_some()
                    })
                })
                .count();
            if owned >= MAX_BRIDGES_PER_IDENTITY {
                return Err(());
            }
        }
        self.generation = self.generation.checked_add(1).ok_or(())?;
        let generation = self.generation;
        let bridge = self
            .bridges
            .entry(token)
            .or_insert_with(|| Bridge { identities, participants: [None, None] });
        let previous = bridge.participants[slot].replace(Participant { generation, value });
        Ok((
            Registration { token, slot, generation },
            previous.map(|participant| participant.value),
        ))
    }

    fn current(&self, registration: &Registration) -> bool {
        self.bridges
            .get(&registration.token)
            .and_then(|bridge| bridge.participants[registration.slot].as_ref())
            .is_some_and(|participant| participant.generation == registration.generation)
    }

    fn destination(&self, registration: &Registration) -> Option<&T> {
        if !self.current(registration) {
            return None;
        }
        self.bridges.get(&registration.token)?.participants[1 - registration.slot]
            .as_ref()
            .map(|participant| &participant.value)
    }

    fn remove(&mut self, registration: &Registration) {
        if !self.current(registration) {
            return;
        }
        if let Some(bridge) = self.bridges.get_mut(&registration.token) {
            bridge.participants[registration.slot] = None;
            if bridge.participants.iter().all(Option::is_none) {
                self.bridges.remove(&registration.token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token admits only the two identities it names, a displaced generation can neither
    /// forward nor remove its replacement, an identity's quota counts only the slots it holds,
    /// and the bridge count stays bounded.
    #[test]
    fn a_bridge_admits_only_its_two_identities_within_their_quotas() {
        let mut routes = Registry::default();
        let (a, _) = routes.register([1; 16], [2; 32], [3; 32], "a").unwrap();
        assert!(routes.destination(&a).is_none());
        assert!(routes.register([1; 16], [4; 32], [2; 32], "stranger").is_err());
        assert!(routes.register([1; 16], [3; 32], [4; 32], "wrong peer").is_err());
        let (b, _) = routes.register([1; 16], [3; 32], [2; 32], "b").unwrap();
        assert_eq!((routes.destination(&a), routes.destination(&b)), (Some(&"b"), Some(&"a")));

        let (newer, displaced) = routes.register([1; 16], [2; 32], [3; 32], "newer").unwrap();
        assert_eq!(displaced, Some("a"));
        assert!(routes.destination(&a).is_none(), "the old generation forwards nothing");
        routes.remove(&a);
        assert_eq!(routes.destination(&b), Some(&"newer"), "nor removes its replacement");
        routes.remove(&newer);
        routes.remove(&b);
        assert!(routes.bridges.is_empty());

        let mut routes = Registry::default();
        for n in 0..MAX_BRIDGES_PER_IDENTITY {
            routes.register([n as u8; 16], [2; 32], [3; 32], n).unwrap();
        }
        assert!(routes.register([90; 16], [2; 32], [3; 32], 90).is_err());
        assert!(
            routes.register([0; 16], [2; 32], [3; 32], 100).is_ok(),
            "a replacement fits at capacity"
        );
        assert!(
            routes.register([90; 16], [3; 32], [2; 32], 90).is_ok(),
            "being named spends no quota"
        );
        assert!(routes.register([90; 16], [2; 32], [3; 32], 91).is_err());
        assert!(routes.register([91; 16], [4; 32], [4; 32], 91).is_err(), "no bridge to oneself");

        let mut routes = Registry::default();
        for n in 0..MAX_BRIDGES as u32 {
            let (mut token, mut identity) = ([0; 16], [0; 32]);
            token[..4].copy_from_slice(&n.to_be_bytes());
            identity[..4].copy_from_slice(&n.to_be_bytes());
            routes.register(token, identity, [255; 32], ()).unwrap();
        }
        assert!(routes.register([254; 16], [254; 32], [255; 32], ()).is_err());
        assert_eq!(routes.bridges.len(), MAX_BRIDGES);
    }
}
