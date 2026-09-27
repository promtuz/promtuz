//! Real transport checks for negotiated ranges, sparse recovery and old peers.
use super::*;
use crate::p2p::{
    PeerLink,
    protocol::{AttachmentProtocol, LEGACY_ALPN, offered_alpns},
};
use ed25519_dalek::{Signer, SigningKey};
use std::net::Ipv6Addr;

const TLS_SEED: [u8; 32] = [0xf4; 32];

pub(super) fn identity(id: u8) -> wire::Auth {
    let mut seed = [0xf5; 32];
    seed[0] = id;
    let key = SigningKey::from_bytes(&seed);
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
