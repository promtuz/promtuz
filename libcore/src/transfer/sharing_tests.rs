//! Separate processes model distinct recipient stores. Only discovery/MLS
//! delivery are fixtures; mutual QUIC authentication, helper authorization,
//! serving, sparse receiver recovery and provider fallback are production code.
use super::*;
use crate::data::conversation::Conversation;
use crate::p2p::{PeerLink, protocol::offered_alpns};
use ed25519_dalek::{Signer, SigningKey};
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

const TEST: &str =
    "transfer::sharing_tests::recipient_copies_fallback_and_revoke_across_separate_stores";
const ROLE: &str = "PROMTUZ_SHARING_TEST_ROLE";
const ROOT: &str = "PROMTUZ_SHARING_TEST_ROOT";
const TLS: [u8; 32] = [0xE1; 32];
const ORIGINAL: u8 = 210;
const RECEIVER: u8 = 211;
const OLD: u8 = 212;
const BAD: u8 = 213;
const GOOD: u8 = 214;

fn identity(id: u8) -> wire::Auth {
    let key = SigningKey::from_bytes(&[id; 32]);
    let tls_pub = SigningKey::from_bytes(&TLS).verifying_key().to_bytes();
    wire::Auth {
        ipk: key.verifying_key().to_bytes(),
        tls_pub,
        sig: key.sign(&crate::quic::peer_config::ipk_binding_message(&tls_pub)).to_bytes(),
    }
}

fn content() -> Vec<u8> {
    (0..6 * wire::CHUNK_SIZE + 31)
        .map(|i| ((i / wire::CHUNK_SIZE) as u8).wrapping_add(37))
        .collect()
}

fn setup(me: u8, root: &Path) -> ([u8; 16], wire::Manifest, [u8; 32], Vec<u8>) {
    let bytes = content();
    let path = root.join(format!("source-{me}"));
    std::fs::write(&path, &bytes).unwrap();
    let manifest = wire::Manifest::from_file(path.to_str().unwrap()).unwrap();
    let fid = manifest.file_id();
    let author = identity(ORIGINAL).ipk;
    let mut recipients =
        vec![identity(RECEIVER).ipk, identity(OLD).ipk, identity(BAD).ipk, identity(GOOD).ipk];
    recipients.sort_unstable();
    let conv = Conversation::join_group(&author, &recipients).unwrap();
    Conversation::bind_group(&conv, &[0xE2; 32]).unwrap();
    crate::data::identity::Identity::save(crate::db::identity::IdentityRow {
        id: 0,
        ipk: identity(me).ipk,
        enc_isk: vec![],
        created_at: 0,
        name: "test".into(),
        avatar: None,
        avatar_revision: 0,
        bio: String::new(),
        profile_revision: 0,
    })
    .unwrap();
    crate::messaging::save_inbound_body(
        &conv,
        &author,
        &[0xE3; 16],
        1000,
        None,
        common::proto::mls_wire::Body::Attachment {
            caption: "file".into(),
            group_id: None,
            mime: "application/octet-stream".into(),
            name: "shared.bin".into(),
            size: bytes.len() as u64,
            thumb: vec![],
            file_id: fid,
        },
    )
    .unwrap();
    // A common fixed expiry makes every actor's authenticated grant identical.
    let expiry: u64 = std::fs::read_to_string(root.join("expiry")).unwrap().parse().unwrap();
    sharing::receive(
        conv,
        author,
        common::proto::mls_wire::AttachmentSharing {
            message_id: [0xE3; 16],
            file_id: fid,
            size: bytes.len() as u64,
            expires_at: expiry,
            recipients,
        },
    )
    .unwrap();
    let grant = crate::db::messages::MESSAGES_DB
        .lock()
        .query_row("SELECT grant_id FROM attachment_sharing", [], |r| r.get(0))
        .unwrap();
    if me != RECEIVER {
        store::partial_put(&store::Partial {
            file_id: fid,
            source_ipk: author,
            total: bytes.len() as u64,
            chunk_size: manifest.chunk_size,
            manifest: Some(postcard::to_allocvec(&manifest).unwrap()),
            have: manifest.chunks.len() as u32,
            state: store::DONE,
            path: path.display().to_string(),
            updated_at: 1000,
        })
        .unwrap();
        if me == BAD {
            let mut damaged = bytes.clone();
            damaged[2 * wire::CHUNK_SIZE] ^= 1;
            std::fs::write(path, damaged).unwrap();
        }
    }
    (conv, manifest, grant, bytes)
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(root: &Path, role: &str) -> Child {
    let profile = root.join(role);
    std::fs::create_dir_all(&profile).unwrap();
    let log = std::fs::File::create(root.join(format!("{role}.log"))).unwrap();
    Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, role)
            .env(ROOT, root)
            .env("PROMTUZ_DATA_DIR", profile)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    )
}

async fn wait_file(path: &Path) {
    timeout(Duration::from_secs(12), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("test actor did not produce {}", path.display()));
}

async fn command(root: &Path, command: &str) {
    let ack = root.join("ack");
    let _ = std::fs::remove_file(&ack);
    std::fs::write(root.join("command.tmp"), command).unwrap();
    std::fs::rename(root.join("command.tmp"), root.join("command")).unwrap();
    wait_file(&ack).await;
}

async fn provider(root: &Path, id: u8) {
    let (conv, manifest, _, _) = setup(id, root);
    let local = identity(id);
    let key = SigningKey::from_bytes(&TLS);
    let (server, _) =
        crate::quic::peer_config::test_peer_configs_with_protocols(&key, offered_alpns()).unwrap();
    let endpoint = quinn::Endpoint::server(server, (Ipv6Addr::LOCALHOST, 0).into()).unwrap();
    sharing::set_foreground(true);
    sharing::set_network(true);
    std::fs::write(root.join(format!("ready-{id}")), endpoint.local_addr().unwrap().to_string())
        .unwrap();
    let mut commands = tokio::time::interval(Duration::from_millis(5));
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let conn = incoming.unwrap().await.unwrap();
                let link = crate::p2p::test_link(conn, identity(RECEIVER).ipk);
                if id == OLD {
                    let local = local.clone(); let marker = root.join("old-no-request");
                    tokio::spawn(async move {
                        let (mut s, mut r) = link.accept_stream().await.unwrap();
                        auth::exchange(&link.conn, &mut s, &mut r, link.ipk, &local).await.unwrap();
                        assert!(matches!(v2::read_frame_for(&mut r, v2::ReadPhase::Hello).await.unwrap(), v2::Frame::Hello(_)));
                        // Consume the client's Hello before advertising the old
                        // capability: the client may reset immediately afterwards.
                        v2::write_frame(&mut s, &v2::Frame::Hello(v2::Hello { supported: 1, required: 1, ..v2::Hello::local() })).await.unwrap();
                        let mut byte = [0];
                        assert!(!matches!(r.read(&mut byte).await, Ok(Some(_))), "new request reached an old peer");
                        std::fs::write(marker, b"ok").unwrap();
                    });
                } else { tokio::spawn(serve_streams(link, local.clone())); }
            },
            _ = commands.tick(), if id == GOOD => {
                if let Ok(action) = std::fs::read_to_string(root.join("command")) {
                    match action.as_str() {
                        "background" => sharing::set_foreground(false),
                        "foreground" => sharing::set_foreground(true),
                        "metered" => sharing::set_network(false),
                        "wifi" => sharing::set_network(true),
                        "remove-author" => {
                            let db = crate::db::messages::MESSAGES_DB.lock();
                            Conversation::deactivate_member_tx(&db, &conv, &identity(ORIGINAL).ipk).unwrap();
                        },
                        "restore-author" => { crate::db::messages::MESSAGES_DB.lock().execute(
                            "UPDATE conversation_members SET active=1 WHERE conversation_id=?1", [conv.as_slice()]).unwrap(); },
                        "delete" => { crate::data::message::Message::receive_delete(&conv, &[0xE3; 16], &identity(ORIGINAL).ipk).unwrap(); },
                        _ => panic!("unknown test command"),
                    }
                    if action != "delete" { assert!(store::partial_get(&manifest.file_id()).unwrap().is_complete()); }
                    std::fs::remove_file(root.join("command")).unwrap();
                    std::fs::write(root.join("ack"), b"ok").unwrap();
                }
            },
        }
    }
}

async fn describe(
    link: &PeerLink, local: &wire::Auth, file_id: [u8; 32], grant: [u8; 32],
) -> v2::Frame {
    let (mut s, mut r, _) = open_v2_request(link, local).await.unwrap();
    v2::write_frame(&mut s, &v2::Frame::DescribeShared { file_id, grant }).await.unwrap();
    s.finish().unwrap();
    v2::read_frame_for(&mut r, v2::ReadPhase::Manifest).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recipient_copies_fallback_and_revoke_across_separate_stores() {
    let _ = common::quic::config::setup_crypto_provider();
    let role = std::env::var(ROLE).unwrap_or_default();
    if role.is_empty() {
        let root = std::env::temp_dir().join(format!("promtuz-sharing-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("expiry"), (crate::utils::systime().as_secs() + 3600).to_string())
            .unwrap();
        let mut child = spawn(&root, "driver");
        let result = timeout(Duration::from_secs(60), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("helper integration test timed out");
        let log = std::fs::read_to_string(root.join("driver.log")).unwrap();
        assert!(result.success(), "{log}\nactor logs: {}", root.display());
        drop(child);
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    let root = PathBuf::from(std::env::var(ROOT).unwrap());
    if role != "driver" {
        provider(&root, role.parse().unwrap()).await;
        return;
    }
    let children: Vec<_> =
        [OLD, BAD, GOOD].iter().map(|id| spawn(&root, &id.to_string())).collect();
    for id in [OLD, BAD, GOOD] {
        wait_file(&root.join(format!("ready-{id}"))).await;
    }
    let (conv, manifest, grant, bytes) = setup(RECEIVER, &root);
    let file = manifest.file_id();
    let local = identity(RECEIVER);
    assert_eq!(
        crate::p2p::consent::may_connect(&identity(GOOD).ipk),
        crate::p2p::consent::Decision::RelayedOnly
    );
    let key = SigningKey::from_bytes(&TLS);
    let (_, mut config) =
        crate::quic::peer_config::test_peer_configs_with_protocols(&key, offered_alpns()).unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window((64u32 * 1024).into());
    transport.receive_window((128u32 * 1024).into());
    config.transport_config(std::sync::Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
    endpoint.set_default_client_config(config);
    let mut links = Vec::new();
    for id in [OLD, BAD, GOOD] {
        let addr: SocketAddr =
            std::fs::read_to_string(root.join(format!("ready-{id}"))).unwrap().parse().unwrap();
        let conn = endpoint.connect(addr, "peer").unwrap().await.unwrap();
        links.push(crate::p2p::test_link(conn, identity(id).ipk));
    }
    let good = links[2].clone();
    assert_eq!(
        describe(&good, &local, file, [0; 32]).await,
        v2::Frame::Error(v2::ErrorCode::Unavailable)
    );
    let lease = store::receiver_lease(file);
    let mut receiver = ranges::Receiver::open_async(
        file,
        identity(ORIGINAL).ipk,
        manifest.clone(),
        bytes.len() as u64,
        &lease,
    )
    .await
    .unwrap();
    for i in [0usize, 5] {
        receiver
            .commit(i as u32, &bytes[i * wire::CHUNK_SIZE..(i + 1) * wire::CHUNK_SIZE], &lease)
            .unwrap();
    }
    drop(receiver);
    let candidates = links.iter().map(|l| (l.ipk, grant)).collect();
    let mut attempts = 0;
    assert!(
        try_helpers(file, bytes.len() as u64, &local, &lease, candidates, |peer| {
            attempts += 1;
            if attempts == 3 {
                assert!(
                    store::verified_count(&store::partial_get(&file).unwrap()) >= 3,
                    "verified bytes from original and damaged helper must survive"
                );
            }
            std::future::ready(Ok(links.iter().find(|l| l.ipk == peer).unwrap().clone()))
        })
        .await
        .unwrap()
    );
    assert_eq!(attempts, 3);
    wait_file(&root.join("old-no-request")).await;
    let done = store::partial_get(&file).unwrap();
    assert!(done.is_complete());
    assert_eq!(std::fs::read(&done.path).unwrap(), bytes);
    assert_eq!(wire::Manifest::from_file(&done.path).unwrap().file_id(), file);
    // Hold the reader after one chunk to force real QUIC backpressure, then
    // background the provider. It must cancel an already-streaming upload.
    let (mut s, mut r, _) = open_v2_request(&good, &local).await.unwrap();
    v2::write_frame(
        &mut s,
        &v2::Frame::PullShared {
            file_id: file,
            grant,
            ranges: vec![ranges::ChunkRange { start: 0, end: manifest.chunks.len() as u32 }],
        },
    )
    .await
    .unwrap();
    s.finish().unwrap();
    assert!(matches!(
        v2::read_frame_for(&mut r, v2::ReadPhase::Chunk).await.unwrap(),
        v2::Frame::Chunk { index: 0, .. }
    ));
    command(&root, "background").await;
    let mut received = 1;
    timeout(Duration::from_secs(2), async {
        while let Ok(v2::Frame::Chunk { .. }) =
            v2::read_frame_for(&mut r, v2::ReadPhase::Chunk).await
        {
            received += 1;
        }
    })
    .await
    .unwrap();
    assert!(received < manifest.chunks.len());
    assert_eq!(
        describe(&good, &local, file, grant).await,
        v2::Frame::Error(v2::ErrorCode::Unavailable)
    );
    command(&root, "foreground").await;
    assert!(matches!(describe(&good, &local, file, grant).await, v2::Frame::Manifest(_)));
    command(&root, "metered").await;
    assert_eq!(
        describe(&good, &local, file, grant).await,
        v2::Frame::Error(v2::ErrorCode::Unavailable)
    );
    command(&root, "wifi").await;
    command(&root, "remove-author").await;
    assert_eq!(
        describe(&good, &local, file, grant).await,
        v2::Frame::Error(v2::ErrorCode::Unavailable)
    );
    command(&root, "restore-author").await;
    command(&root, "delete").await;
    assert_eq!(
        describe(&good, &local, file, grant).await,
        v2::Frame::Error(v2::ErrorCode::Unavailable)
    );
    assert!(
        store::partial_get(&file).unwrap().is_complete(),
        "provider revocation does not erase the recipient's own copy"
    );
    Conversation::delete(&conv).unwrap();
    endpoint.close(0u32.into(), b"test complete");
    drop(children);
}
