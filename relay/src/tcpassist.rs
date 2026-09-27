//! Authenticated TLS-only attachment bridges. The legacy UDP bearer-token
//! table is deliberately separate: knowing a token cannot claim an endpoint
//! registered to another identity here.

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
                // Nonblocking enqueue while the generation check is locked
                // makes replacement and forwarding linearizable. A full
                // receiver queue drops this datagram; no other route waits.
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
        // Joining an existing bridge also consumes a participant slot. Count
        // live participants, not targets someone else named: an attacker
        // cannot spend another IPK's quota by naming it as its remote.
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
    use common::quic::tunnel;
    use ed25519_dalek::{Signer, SigningKey};
    use std::time::Duration;

    struct TestRelay {
        address: std::net::SocketAddr,
        roots: rustls::RootCertStore,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn test_relay() -> TestRelay {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let mut config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert], key.into())
                .unwrap();
        config.alpn_protocols = vec![tunnel::ALPN.to_vec()];
        let config = Arc::new(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let assist = Arc::new(Assist::default());
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = tasks.join_next(), if !tasks.is_empty() => {},
                    stream = listener.accept() => {
                        let (stream, _) = stream.unwrap();
                        let config = config.clone();
                        let assist = assist.clone();
                        tasks.spawn(async move {
                            let accepted = tunnel::accept(stream, config, tunnel::FEATURE_ASSIST).await.unwrap();
                            assist.serve(accepted.channel, accepted.mode).await;
                        });
                    },
                }
            }
        });
        TestRelay { address, roots, task }
    }

    async fn join(
        relay: &TestRelay, token: [u8; 16], key: &SigningKey, peer: &SigningKey,
    ) -> Arc<Channel> {
        let ipk = key.verifying_key().to_bytes();
        let peer = peer.verifying_key().to_bytes();
        let key = key.clone();
        tunnel::connect(
            relay.address,
            "localhost",
            &relay.roots,
            tunnel::Request::Assist {
                token,
                ipk,
                peer,
                sign: Arc::new(move |message| Ok(key.sign(message).to_bytes())),
            },
        )
        .await
        .unwrap()
    }

    async fn delivered(from: &Channel, to: &Channel, payload: &[u8]) {
        // Admission is sent before the bridge task is scheduled. A bounded
        // repeat matches QUIC loss/retransmit semantics at this datagram API.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                from.try_send(payload).unwrap();
                if let Ok(packet) = tokio::time::timeout(Duration::from_millis(30), to.recv()).await
                {
                    assert_eq!(packet.unwrap(), payload);
                    return;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_tls_bridge_replaces_only_its_authenticated_participant() {
        let relay = test_relay().await;
        let a = SigningKey::from_bytes(&[10; 32]);
        let b = SigningKey::from_bytes(&[11; 32]);
        let stranger = SigningKey::from_bytes(&[12; 32]);
        let a1 = join(&relay, [1; 16], &a, &b).await;
        let b1 = join(&relay, [1; 16], &b, &a).await;
        delivered(&a1, &b1, b"opaque peer QUIC").await;
        delivered(&b1, &a1, b"opaque return path").await;

        let intruder = join(&relay, [1; 16], &stranger, &a).await;
        tokio::time::timeout(Duration::from_secs(3), intruder.closed()).await.unwrap();
        delivered(&a1, &b1, b"token did not admit stranger").await;

        let inconsistent = join(&relay, [1; 16], &b, &stranger).await;
        tokio::time::timeout(Duration::from_secs(3), inconsistent.closed()).await.unwrap();
        delivered(&b1, &a1, b"incorrect peer did not replace b").await;

        let a2 = join(&relay, [1; 16], &a, &b).await;
        tokio::time::timeout(Duration::from_secs(3), a1.closed()).await.unwrap();
        assert!(a1.try_send(b"old generation").is_err());
        delivered(&b1, &a2, b"new generation receives").await;
        delivered(&a2, &b1, b"new generation sends").await;
        a2.close();
        b1.close();
    }

    #[test]
    fn token_holder_cannot_join_or_replace_another_identity() {
        let mut routes = Registry::default();
        let (a, _) = routes.register([1; 16], [2; 32], [3; 32], "a").unwrap();
        assert!(routes.destination(&a).is_none());
        assert!(routes.register([1; 16], [4; 32], [2; 32], "attacker").is_err());
        assert!(routes.register([1; 16], [3; 32], [4; 32], "wrong peer").is_err());
        let (b, _) = routes.register([1; 16], [3; 32], [2; 32], "b").unwrap();
        assert_eq!(routes.destination(&a), Some(&"b"));
        assert_eq!(routes.destination(&b), Some(&"a"));
    }

    #[test]
    fn stale_registration_can_neither_forward_nor_remove_its_replacement() {
        let mut routes = Registry::default();
        let (old, _) = routes.register([1; 16], [2; 32], [3; 32], "old").unwrap();
        let (b, _) = routes.register([1; 16], [3; 32], [2; 32], "b").unwrap();
        let (new, displaced) = routes.register([1; 16], [2; 32], [3; 32], "new").unwrap();
        assert_eq!(displaced, Some("old"));
        assert!(routes.destination(&old).is_none());
        routes.remove(&old);
        assert_eq!(routes.destination(&b), Some(&"new"));
        routes.remove(&new);
        assert!(routes.destination(&b).is_none());
        routes.remove(&b);
        assert!(routes.bridges.is_empty());
    }

    #[test]
    fn quota_counts_owned_slots_and_permits_replacement_at_capacity() {
        let mut routes = Registry::default();
        for n in 0..MAX_BRIDGES_PER_IDENTITY {
            routes.register([n as u8; 16], [2; 32], [3; 32], n).unwrap();
        }
        assert!(routes.register([90; 16], [2; 32], [3; 32], 90).is_err());
        assert!(routes.register([0; 16], [2; 32], [3; 32], 100).is_ok());
        // Naming someone as the remote identity cannot consume their budget.
        assert!(routes.register([90; 16], [3; 32], [2; 32], 90).is_ok());
        assert!(routes.register([90; 16], [2; 32], [3; 32], 91).is_err());
        assert!(routes.register([91; 16], [4; 32], [4; 32], 91).is_err());
    }

    #[test]
    fn global_bridge_count_stays_bounded() {
        let mut routes = Registry::default();
        for n in 0..MAX_BRIDGES as u32 {
            let mut token = [0; 16];
            token[..4].copy_from_slice(&n.to_be_bytes());
            let mut identity = [0; 32];
            identity[..4].copy_from_slice(&n.to_be_bytes());
            routes.register(token, identity, [255; 32], ()).unwrap();
        }
        assert!(routes.register([254; 16], [254; 32], [255; 32], ()).is_err());
        assert_eq!(routes.bridges.len(), MAX_BRIDGES);
    }
}
