//! Real transport checks for negotiated ranges, sparse recovery and old peers.
use super::*;
use crate::p2p::{
    PeerLink,
    protocol::{AttachmentProtocol, LEGACY_ALPN, offered_alpns},
};
use ed25519_dalek::{Signer, SigningKey};
use std::net::Ipv6Addr;

const TLS_SEED: [u8; 32] = [0xf4; 32];

fn identity_key(id: u8) -> SigningKey {
    let mut seed = [0xf5; 32];
    seed[0] = id;
    SigningKey::from_bytes(&seed)
}

pub(super) fn identity(id: u8) -> wire::Auth {
    let key = identity_key(id);
    let tls_pub = SigningKey::from_bytes(&TLS_SEED).verifying_key().to_bytes();
    let auth = wire::Auth {
        ipk: key.verifying_key().to_bytes(),
        tls_pub,
        sig: key.sign(&crate::quic::peer_config::ipk_binding_message(&tls_pub)).to_bytes(),
    };
    crate::data::contact::Contact::save_pending(auth.ipk, "range test".into()).unwrap();
    crate::data::contact::Contact::mark_paired(&auth.ipk);
    auth
}

/// Production TLS admission, PunchSocket routing and peer QUIC. Only the relay
/// pairing/forward loop is a fixture here; production registry generations and
/// quotas have their own relay-side tests. No MLS offer exchange is simulated.
struct TcpPair {
    server: PeerLink,
    client: PeerLink,
    endpoints: [quinn::Endpoint; 2],
    channels: [std::sync::Arc<common::quic::tunnel::Channel>; 2],
    tasks: Vec<tokio::task::JoinHandle<()>>,
    udp_packets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for TcpPair {
    fn drop(&mut self) {
        for endpoint in &self.endpoints {
            endpoint.close(0u32.into(), b"test complete");
        }
        for channel in &self.channels {
            channel.close();
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn linked_tcp(
    sender_id: u8, receiver_id: u8, token_id: u8, pause_after_bytes: Option<usize>,
) -> TcpPair {
    use common::quic::tunnel::{self, AcceptedMode, Channel, Request};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["relay.test".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut tls = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    tls.alpn_protocols = vec![tunnel::ALPN.to_vec()];
    let tls = Arc::new(tls);
    let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
    let relay = listener.local_addr().unwrap();
    let blackhole = tokio::net::UdpSocket::bind(relay).await.unwrap();
    let udp_packets = Arc::new(AtomicUsize::new(0));
    let dropping = tokio::spawn({
        let udp_packets = udp_packets.clone();
        async move {
            let mut bytes = [0u8; 4096];
            while blackhole.recv_from(&mut bytes).await.is_ok() {
                udp_packets.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    let sender_key = identity_key(sender_id);
    let receiver_key = identity_key(receiver_id);
    let sender = sender_key.verifying_key().to_bytes();
    let receiver = receiver_key.verifying_key().to_bytes();
    let token = [token_id; 16];
    async fn forward(from: &Arc<Channel>, to: &Arc<Channel>, pause_after_bytes: Option<usize>) {
        let mut writable = to.clone().create_io_poller();
        let mut forwarded = 0;
        while let Ok(packet) = from.recv().await {
            loop {
                match to.try_send(&packet) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::future::poll_fn(|cx| writable.as_mut().poll_writable(cx))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    },
                    Err(_) => return,
                }
            }
            forwarded += packet.len();
            if pause_after_bytes.is_some_and(|budget| forwarded >= budget) {
                // An opaque byte gate permits the first durable chunk but
                // prevents finishing the file before the interruption task
                // gets scheduled. It does not inspect or replace transfer frames.
                from.closed().await;
                return;
            }
        }
    }
    let serving = tokio::spawn(async move {
        let first =
            tunnel::accept(listener.accept().await.unwrap().0, tls.clone(), tunnel::FEATURE_ASSIST)
                .await
                .unwrap();
        let second =
            tunnel::accept(listener.accept().await.unwrap().0, tls, tunnel::FEATURE_ASSIST)
                .await
                .unwrap();
        assert_eq!(first.mode, AcceptedMode::Assist { token, ipk: sender, peer: receiver });
        assert_eq!(second.mode, AcceptedMode::Assist { token, ipk: receiver, peer: sender });
        tokio::select! {
            _ = forward(&first.channel, &second.channel, pause_after_bytes) => {},
            _ = forward(&second.channel, &first.channel, None) => {},
        }
    });
    let a = tunnel::connect(
        relay,
        "relay.test",
        &roots,
        Request::Assist {
            token,
            ipk: sender,
            peer: receiver,
            sign: Arc::new(move |message| Ok(sender_key.sign(message).to_bytes())),
        },
    )
    .await
    .unwrap();
    let b = tunnel::connect(
        relay,
        "relay.test",
        &roots,
        Request::Assist {
            token,
            ipk: receiver,
            peer: sender,
            sign: Arc::new(move |message| Ok(receiver_key.sign(message).to_bytes())),
        },
    )
    .await
    .unwrap();
    let key = SigningKey::from_bytes(&TLS_SEED);
    let (server_endpoint, _) =
        crate::p2p::test_tcp_endpoint(a.clone(), &key, relay, token).unwrap();
    let (client_endpoint, synth) =
        crate::p2p::test_tcp_endpoint(b.clone(), &key, relay, token).unwrap();
    let (server_conn, client_conn) = timeout(Duration::from_secs(8), async {
        tokio::join!(async { server_endpoint.accept().await.unwrap().await.unwrap() }, async {
            client_endpoint.connect(synth, "peer").unwrap().await.unwrap()
        },)
    })
    .await
    .unwrap();
    assert!(
        udp_packets.load(Ordering::Relaxed) > 0,
        "native UDP was attempted but never forwarded"
    );
    TcpPair {
        server: crate::p2p::test_link(server_conn, receiver),
        client: crate::p2p::test_link(client_conn, sender),
        endpoints: [server_endpoint, client_endpoint],
        channels: [a, b],
        tasks: vec![dropping, serving],
        udp_packets,
    }
}

#[tokio::test]
async fn attachment_ranges_resume_after_tcp_only_link_interruption_and_match_original_file() {
    let sender = identity(91);
    let receiver = identity(92);
    let mut bytes = vec![0u8; wire::CHUNK_SIZE * 6 + 517];
    for (index, chunk) in bytes.chunks_mut(wire::CHUNK_SIZE).enumerate() {
        chunk.fill(0x91u8.wrapping_add(index as u8));
    }
    let path =
        std::env::temp_dir().join(format!("promtuz-tcp-attachment-{}.bin", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    let (fid, size) = prepare_send(path.to_str().unwrap(), 3600).unwrap();
    store::forget_partial(&fid);
    offer(&receiver.ipk, &fid);
    let manifest = wire::Manifest::from_file(path.to_str().unwrap()).unwrap();
    let lease = store::receiver_lease(fid);
    let mut partial =
        ranges::Receiver::open(fid, sender.ipk, manifest.clone(), size, &lease).unwrap();
    partial.commit(2, &bytes[2 * wire::CHUNK_SIZE..3 * wire::CHUNK_SIZE], &lease).unwrap();
    drop(partial);

    let first = linked_tcp(91, 92, 94, Some(wire::CHUNK_SIZE * 2)).await;
    let second = linked_tcp(91, 92, 95, None).await;
    assert_eq!(first.client.protocol().unwrap(), AttachmentProtocol::V2);
    let serve_first = tokio::spawn(serve_streams(first.server.clone(), sender.clone()));
    let serve_second = tokio::spawn(serve_streams(second.server.clone(), sender.clone()));
    let cut = tokio::spawn({
        let channel = first.channels[1].clone();
        let endpoint = first.endpoints[1].clone();
        async move {
            timeout(Duration::from_secs(8), async {
                while !store::partial_get(&fid).is_some_and(|p| p.have >= 1) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // Emulate the A1 network-generation teardown after durable progress.
            // Recovery detection/timer policy itself has separate tests.
            channel.close();
            endpoint.close(0u32.into(), b"test network generation changed");
        }
    });
    let mut links = std::collections::VecDeque::from([first.client.clone(), second.client.clone()]);
    let mut attempts = 0;
    let exhausted = timeout(
        Duration::from_secs(20),
        drive_download(
            fid,
            sender.ipk,
            size,
            &receiver,
            &lease,
            &[Duration::from_millis(1)],
            || {
                attempts += 1;
                if attempts == 2 {
                    let partial = store::partial_get(&fid).unwrap();
                    assert!(partial.have > 0 && partial.have < manifest.chunks.len() as u32);
                    assert!(
                        store::verified_count(&partial) >= 2,
                        "sparse verified progress survived"
                    );
                }
                std::future::ready(Ok(links.pop_front().expect("only one reconnect is allowed")))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    cut.await.unwrap();
    serve_first.abort();
    serve_second.abort();
    assert!(!exhausted);
    assert_eq!(attempts, 2);
    let partial = store::partial_get(&fid).unwrap();
    assert_eq!(partial.state, store::DONE);
    assert_eq!(store::verified_count(&partial), manifest.chunks.len() as u32);
    let received = std::fs::read(&partial.path).unwrap();
    assert_eq!(received, bytes);
    assert_eq!(blake3::hash(&received), blake3::hash(&bytes));
    assert!(first.udp_packets.load(std::sync::atomic::Ordering::Relaxed) > 0);
    assert!(second.udp_packets.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

pub(super) async fn linked(
    sender: &wire::Auth, receiver: &wire::Auth, server_protocols: Vec<Vec<u8>>,
    client_protocols: Vec<Vec<u8>>,
) -> (PeerLink, PeerLink, quinn::Endpoint, quinn::Endpoint) {
    let _ = common::quic::config::setup_crypto_provider();
    let key = SigningKey::from_bytes(&TLS_SEED);
    let (server, _) =
        crate::quic::peer_config::test_peer_configs_with_protocols(&key, server_protocols).unwrap();
    let (_, mut client) =
        crate::quic::peer_config::test_peer_configs_with_protocols(&key, client_protocols).unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window((64u32 * 1024).into());
    transport.receive_window((128u32 * 1024).into());
    client.transport_config(std::sync::Arc::new(transport));
    let a = quinn::Endpoint::server(server, (Ipv6Addr::LOCALHOST, 0).into()).unwrap();
    let mut b = quinn::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
    b.set_default_client_config(client);
    let dial = b.connect(a.local_addr().unwrap(), "peer").unwrap();
    let (accept, dial) = timeout(Duration::from_secs(5), async {
        tokio::join!(async { a.accept().await.unwrap().accept().unwrap().await.unwrap() }, async {
            dial.await.unwrap()
        })
    })
    .await
    .unwrap();
    (crate::p2p::test_link(accept, receiver.ipk), crate::p2p::test_link(dial, sender.ipk), a, b)
}

pub(super) fn fixture(id: u8, chunks: usize) -> (Vec<u8>, wire::Manifest) {
    let mut bytes = Vec::new();
    for idx in 0..chunks {
        bytes.extend(vec![id.wrapping_add(idx as u8); if idx + 1 == chunks { 517 } else { 1024 }]);
    }
    let manifest = wire::Manifest {
        total_size: bytes.len() as u64,
        chunk_size: 1024,
        chunks: bytes.chunks(1024).map(|chunk| *blake3::hash(chunk).as_bytes()).collect(),
    };
    let path = std::env::temp_dir().join(format!("promtuz-range-{}-{id}.bin", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    let fid = manifest.file_id();
    store::retention_put(
        &fid,
        path.to_str().unwrap(),
        manifest.total_size,
        manifest.chunk_size,
        &postcard::to_allocvec(&manifest).unwrap(),
        crate::utils::systime().as_secs() + 3600,
    )
    .unwrap();
    store::forget_partial(&fid);
    // Prior fixture messages must not authorize another identity on a later run.
    crate::db::messages::MESSAGES_DB
        .lock()
        .execute("DELETE FROM message_media WHERE file_id=?1", [fid.as_slice()])
        .unwrap();
    (bytes, manifest)
}

pub(super) async fn request(
    link: &PeerLink, local: &wire::Auth,
) -> (quinn::SendStream, quinn::RecvStream, v2::Frame) {
    let (mut s, mut r) =
        timeout(Duration::from_secs(5), link.accept_stream()).await.unwrap().unwrap();
    auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local).await.unwrap();
    v2::exchange_hello(&mut s, &mut r).await.unwrap();
    let frame = v2::read_frame(&mut r).await.unwrap();
    expect_fin(&mut r).await.unwrap();
    (s, r, frame)
}

fn offer(peer: &[u8; 32], fid: &[u8; 32]) {
    let conv = crate::data::conversation::Conversation::for_peer(peer).unwrap();
    super::download_resume::offer_in(conv, *fid);
}

async fn describe(link: &PeerLink, auth: &wire::Auth, manifest: &wire::Manifest) {
    let (mut s, _r, frame) = request(link, auth).await;
    assert_eq!(frame, v2::Frame::Describe { file_id: manifest.file_id() });
    v2::write_frame(&mut s, &v2::Frame::Manifest(manifest.clone())).await.unwrap();
    v2::write_frame(&mut s, &v2::Frame::Complete { chunks_sent: 0 }).await.unwrap();
    s.finish().unwrap();
}

async fn send_chunk(s: &mut quinn::SendStream, index: u32, bytes: &[u8]) {
    let offset = index as usize * 1024;
    let end = (offset + 1024).min(bytes.len());
    v2::write_frame(s, &v2::Frame::Chunk { index, bytes: bytes[offset..end].to_vec() })
        .await
        .unwrap();
}

#[tokio::test]
async fn v2_original_sender_serves_multiple_bounded_batches_and_empty_file() {
    let sender = identity(1);
    let receiver = identity(2);
    let (server, client, _a, _b) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    assert_eq!(client.protocol().unwrap(), AttachmentProtocol::V2);
    let task = tokio::spawn(serve_streams(server, sender));
    for (id, chunks) in [(1, 70), (2, 0)] {
        let (bytes, manifest) = fixture(id, chunks);
        let fid = manifest.file_id();
        offer(&receiver.ipk, &fid);
        timeout(Duration::from_secs(10), pull(&client, fid, manifest.total_size, &receiver))
            .await
            .unwrap()
            .unwrap();
        let p = store::partial_get(&fid).unwrap();
        assert_eq!(p.state, store::DONE);
        assert_eq!(p.have as usize, chunks);
        assert_eq!(store::verified_count(&p) as usize, chunks);
        assert_eq!(std::fs::read(&p.path).unwrap(), bytes);
    }
    task.abort();
}

#[tokio::test]
async fn sparse_ranges_survive_interruption_and_request_only_missing_chunks() {
    let sender = identity(3);
    let receiver = identity(4);
    let (bytes, manifest) = fixture(3, 5);
    let fid = manifest.file_id();
    let lease = store::receiver_lease(fid);
    let mut partial =
        ranges::Receiver::open(fid, sender.ipk, manifest.clone(), manifest.total_size, &lease)
            .unwrap();
    partial.commit(2, &bytes[2048..3072], &lease).unwrap();
    drop(partial);
    let (server1, client1, _a, _b) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let (server2, client2, _c, _d) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let m1 = manifest.clone();
    let auth1 = sender.clone();
    let b1 = bytes.clone();
    let first = tokio::spawn(async move {
        describe(&server1, &auth1, &m1).await;
        let (mut s, _r, frame) = request(&server1, &auth1).await;
        assert_eq!(
            frame,
            v2::Frame::Pull {
                file_id: fid,
                ranges: vec![
                    ranges::ChunkRange { start: 0, end: 2 },
                    ranges::ChunkRange { start: 3, end: 5 }
                ]
            }
        );
        send_chunk(&mut s, 0, &b1).await;
        timeout(Duration::from_secs(3), async {
            while store::partial_get(&fid).unwrap().have < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        server1.conn.close(0u32.into(), b"test link interrupted");
    });
    let m2 = manifest.clone();
    let auth2 = sender.clone();
    let b2 = bytes.clone();
    let second = tokio::spawn(async move {
        describe(&server2, &auth2, &m2).await;
        let (mut s, _r, frame) = request(&server2, &auth2).await;
        assert_eq!(
            frame,
            v2::Frame::Pull {
                file_id: fid,
                ranges: vec![
                    ranges::ChunkRange { start: 1, end: 2 },
                    ranges::ChunkRange { start: 3, end: 5 }
                ]
            }
        );
        for index in [1, 3, 4] {
            send_chunk(&mut s, index, &b2).await;
        }
        v2::write_frame(&mut s, &v2::Frame::Complete { chunks_sent: 3 }).await.unwrap();
        s.finish().unwrap();
        s.stopped().await.unwrap();
    });
    let mut links = std::collections::VecDeque::from([client1, client2]);
    let mut attempts = 0;
    let exhausted = timeout(
        Duration::from_secs(10),
        drive_download(
            fid,
            sender.ipk,
            manifest.total_size,
            &receiver,
            &lease,
            &[Duration::from_millis(1)],
            || {
                attempts += 1;
                std::future::ready(Ok(links.pop_front().unwrap()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!exhausted);
    assert_eq!(attempts, 2);
    first.await.unwrap();
    second.await.unwrap();
    let p = store::partial_get(&fid).unwrap();
    assert_eq!(p.state, store::DONE);
    assert_eq!(std::fs::read(p.path).unwrap(), bytes);
}

#[tokio::test]
async fn old_and_new_peers_transfer_real_bytes_with_sparse_legacy_resume() {
    let sender = identity(5);
    let receiver = identity(6);
    for (id, server_alpns, client_alpns) in [
        (4, vec![LEGACY_ALPN.to_vec()], offered_alpns()),
        (5, offered_alpns(), vec![LEGACY_ALPN.to_vec()]),
    ] {
        let (bytes, m) = fixture(id, 4);
        let fid = m.file_id();
        offer(&receiver.ipk, &fid);
        let lease = store::receiver_lease(fid);
        let mut partial =
            ranges::Receiver::open(fid, sender.ipk, m.clone(), m.total_size, &lease).unwrap();
        partial.commit(0, &bytes[..1024], &lease).unwrap();
        partial.commit(2, &bytes[2048..3072], &lease).unwrap();
        drop(partial);
        drop(lease);
        let (server, client, _a, _b) = linked(&sender, &receiver, server_alpns, client_alpns).await;
        assert_eq!(client.protocol().unwrap(), AttachmentProtocol::Legacy);
        let task = tokio::spawn(serve_streams(server, sender.clone()));
        timeout(Duration::from_secs(5), pull(&client, fid, m.total_size, &receiver))
            .await
            .unwrap()
            .unwrap();
        let p = store::partial_get(&fid).unwrap();
        assert_eq!(p.state, store::DONE);
        assert_eq!(p.have, 4);
        assert_eq!(std::fs::read(&p.path).unwrap(), bytes);
        task.abort();
    }
}

#[tokio::test]
async fn v2_authorization_precedes_metadata_and_is_rechecked_on_existing_link() {
    use crate::data::conversation::Conversation;
    let sender = identity(7);
    let receiver = identity(8);
    let (_bytes, m) = fixture(6, 2);
    let fid = m.file_id();
    let (server, client, _a, _b) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let task = tokio::spawn(serve_streams(server, sender.clone()));
    // Even a mutually authenticated paired peer cannot learn the manifest by hash.
    let e = pull(&client, fid, m.total_size, &receiver).await.unwrap_err();
    assert_eq!(e.kind, FailureKind::Unavailable);
    assert!(store::partial_get(&fid).is_none());
    let group = Conversation::join_group(&sender.ipk, &[sender.ipk, receiver.ipk]).unwrap();
    super::download_resume::offer_in(group, fid);
    let (mut s, mut r, _) = open_v2_request(&client, &receiver).await.unwrap();
    v2::write_frame(&mut s, &v2::Frame::Describe { file_id: fid }).await.unwrap();
    s.finish().unwrap();
    assert_eq!(v2::read_frame(&mut r).await.unwrap(), v2::Frame::Manifest(m.clone()));
    assert_eq!(v2::read_frame(&mut r).await.unwrap(), v2::Frame::Complete { chunks_sent: 0 });
    expect_fin(&mut r).await.unwrap();
    Conversation::deactivate_member(&group, &receiver.ipk).unwrap();
    let (mut s, mut r, _) = open_v2_request(&client, &receiver).await.unwrap();
    v2::write_frame(
        &mut s,
        &v2::Frame::Pull { file_id: fid, ranges: vec![ranges::ChunkRange { start: 0, end: 2 }] },
    )
    .await
    .unwrap();
    s.finish().unwrap();
    assert_eq!(v2::read_frame(&mut r).await.unwrap(), v2::Frame::Error(v2::ErrorCode::Unavailable));
    expect_fin(&mut r).await.unwrap();
    assert!(store::retention_get(&fid).is_some());
    task.abort();
}

#[tokio::test]
async fn modified_sender_file_returns_storage_error_after_valid_chunks() {
    let sender = identity(9);
    let receiver = identity(10);
    let (bytes, m) = fixture(7, 3);
    let fid = m.file_id();
    offer(&receiver.ipk, &fid);
    let retained = store::retention_get(&fid).unwrap();
    let mut edited = bytes.clone();
    edited[1024] ^= 1;
    std::fs::write(&retained.path, edited).unwrap();
    let (server, client, _a, _b) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let task = tokio::spawn(serve_streams(server, sender));
    let e = pull(&client, fid, m.total_size, &receiver).await.unwrap_err();
    assert_eq!(e.kind, FailureKind::Storage);
    let p = store::partial_get(&fid).unwrap();
    assert_eq!(p.have, 1);
    assert_ne!(p.state, store::DONE);
    assert_eq!(std::fs::read(p.path).unwrap(), bytes[..1024]);
    task.abort();
}

#[tokio::test]
async fn group_revocation_interrupts_a_flow_controlled_v2_response() {
    use crate::data::conversation::Conversation;
    let sender = identity(11);
    let receiver = identity(12);
    let bytes = vec![0x6d; 8 * wire::CHUNK_SIZE];
    let path =
        std::env::temp_dir().join(format!("promtuz-range-revocation-{}.bin", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let (fid, _) = prepare_send(path.to_str().unwrap(), 3600).unwrap();
    crate::db::messages::MESSAGES_DB
        .lock()
        .execute("DELETE FROM message_media WHERE file_id=?1", [fid.as_slice()])
        .unwrap();
    let group = Conversation::join_group(&sender.ipk, &[sender.ipk, receiver.ipk]).unwrap();
    super::download_resume::offer_in(group, fid);
    let (server, client, _a, _b) =
        linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let task = tokio::spawn(serve_streams(server, sender));
    let (mut s, mut r, _) = open_v2_request(&client, &receiver).await.unwrap();
    v2::write_frame(
        &mut s,
        &v2::Frame::Pull { file_id: fid, ranges: vec![ranges::ChunkRange { start: 0, end: 8 }] },
    )
    .await
    .unwrap();
    s.finish().unwrap();
    let mut received = 0;
    timeout(Duration::from_secs(5), async {
        loop {
            match v2::read_frame(&mut r).await.unwrap() {
                v2::Frame::Chunk { index, .. } => {
                    assert_eq!(index, received);
                    received += 1;
                    if received == 1 {
                        Conversation::deactivate_member(&group, &receiver.ipk).unwrap();
                    }
                },
                v2::Frame::Error(v2::ErrorCode::Unavailable) => break,
                frame => panic!("expected revocation before completion, got {frame:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        (1..8).contains(&received),
        "already-buffered chunks may arrive; the whole response must not"
    );
    expect_fin(&mut r).await.unwrap();
    task.abort();
}
