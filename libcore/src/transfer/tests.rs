use std::time::Duration;

use common::utils::now_secs;

use super::pull::DOWNLOADING;
use super::pull::DownloadTrigger;
use super::pull::PullGuard;
use super::pull::RETRY_DELAYS;
use super::pull::download_with_policy;
use super::pull::drive_download_inner;
use super::pull::open_v2_request;
use super::pull::pull_live;
use super::pull::set_state;
use super::ranges::ChunkRange;
use super::serve::serve_streams;
use super::v2::ErrorCode;
use super::v2::Frame;
use super::v2::ReadPhase;
use super::*;
use crate::data::conversation::Conversation;
use crate::p2p::PeerLink;
use crate::p2p::protocol::offered_alpns;
use crate::state::Core;
use crate::test_support::transfer::content;
use crate::test_support::transfer::device;
use crate::test_support::transfer::identity;
use crate::test_support::transfer::linked;
use crate::test_support::transfer::manifest;

/// One pull of `file` over `link`, as the download driver runs each attempt.
async fn pull(
    c: &'static Core, link: &PeerLink, file: &wire::Manifest, local: &wire::Auth,
) -> Result<(), Failure> {
    let lease = store::receiver_lease(&c.db, file.file_id());
    pull_live(c, link, file.file_id(), file.total_size, local, &lease).await
}

/// The serving side of one v2 request, scripted by the test.
async fn request(
    link: &PeerLink, local: &wire::Auth,
) -> (quinn::SendStream, quinn::RecvStream, Frame) {
    let (mut s, mut r) = link.accept_stream().await.unwrap();
    auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local).await.unwrap();
    v2::exchange_hello(&mut s, &mut r).await.unwrap();
    let frame = v2::read_frame_for(&mut r, ReadPhase::Request).await.unwrap();
    expect_fin(&mut r).await.unwrap();
    (s, r, frame)
}

async fn describe(link: &PeerLink, local: &wire::Auth, manifest: &wire::Manifest) {
    let (mut s, _r, frame) = request(link, local).await;
    assert_eq!(frame, Frame::Describe { file_id: manifest.file_id() });
    v2::write_frame(&mut s, &Frame::Manifest(manifest.clone())).await.unwrap();
    v2::write_frame(&mut s, &Frame::Complete { chunks_sent: 0 }).await.unwrap();
    s.finish().unwrap();
}

fn chunk(manifest: &wire::Manifest, bytes: &[u8], index: u32) -> Frame {
    let start = index as usize * manifest.chunk_size as usize;
    let end = (start + manifest.chunk_size as usize).min(bytes.len());
    Frame::Chunk { index, bytes: bytes[start..end].to_vec() }
}

fn ranges(start: u32, end: u32) -> Vec<ChunkRange> {
    vec![ChunkRange { start, end }]
}

#[tokio::test]
async fn group_media_without_pairing_is_scoped_and_revocable() {
    let (sender, receiver) = (identity(81), identity(82));
    let (s, r) = (device(), device());
    let group = s.group(&[sender.ipk, receiver.ipk]);
    let file = s.retain(&content(1, 3), 1024);
    s.offer(sender.ipk, group, &file);
    let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let serving = tokio::spawn(serve_streams(s.core, link.server.clone(), sender.clone()));

    pull(r.core, &link.client, &file, &receiver).await.unwrap();
    let done = r.partial(&file.file_id()).unwrap();
    assert!(done.is_complete());
    assert_eq!(std::fs::read(&done.path).unwrap(), content(1, 3));

    let unavailable =
        async |file| pull(r.core, &link.client, file, &receiver).await.unwrap_err().kind;
    let other = s.group(&[sender.ipk, identity(83).ipk]);
    let private = s.retain(&content(2, 3), 1024);
    s.offer(sender.ipk, other, &private);
    assert_eq!(unavailable(&private).await, FailureKind::Unavailable, "a known hash is not access");
    assert!(r.partial(&private.file_id()).is_none());

    let direct =
        Conversation::for_peer_tx(&s.core.db.messages().lock(), &receiver.ipk, Some(sender.ipk));
    s.offer(sender.ipk, direct.unwrap(), &file);
    Conversation::deactivate_member_tx(&s.core.db.messages().lock(), &group, &receiver.ipk)
        .unwrap();
    assert_eq!(unavailable(&file).await, FailureKind::Unavailable, "removed, and never paired");
    let member = crate::mls::policy::ROLE_MEMBER;
    Conversation::put_member(&s.core.db.messages().lock(), &group, &receiver.ipk, member).unwrap();
    Conversation::deactivate_member_tx(&s.core.db.messages().lock(), &group, &sender.ipk).unwrap();
    assert_eq!(
        unavailable(&file).await,
        FailureKind::Unavailable,
        "a sender who left stops serving"
    );
    let kept = store::retention_get_tx(&s.core.db.transfers().lock(), &file.file_id());
    assert!(kept.is_some(), "revocation keeps the sender's copy");
    serving.abort();
}

#[tokio::test]
async fn v2_authorization_precedes_metadata_and_is_rechecked_on_existing_link() {
    let (sender, receiver) = (identity(84), identity(85));
    let (s, r) = (device(), device());
    s.pair(receiver.ipk);
    let file = s.retain(&content(3, 2), 1024);
    let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let serving = tokio::spawn(serve_streams(s.core, link.server.clone(), sender.clone()));

    let refused = pull(r.core, &link.client, &file, &receiver).await.unwrap_err();
    assert_eq!(refused.kind, FailureKind::Unavailable, "a paired peer learns nothing by hash");
    assert!(r.partial(&file.file_id()).is_none());

    let group = s.group(&[sender.ipk, receiver.ipk]);
    s.offer(sender.ipk, group, &file);
    let (mut tx, mut rx, _) = open_v2_request(&link.client, &receiver).await.unwrap();
    v2::write_frame(&mut tx, &Frame::Describe { file_id: file.file_id() }).await.unwrap();
    tx.finish().unwrap();
    let described = v2::read_frame_for(&mut rx, ReadPhase::Manifest).await.unwrap();
    assert_eq!(described, Frame::Manifest(file.clone()));
    let complete = v2::read_frame_for(&mut rx, ReadPhase::Complete).await.unwrap();
    assert_eq!(complete, Frame::Complete { chunks_sent: 0 });
    expect_fin(&mut rx).await.unwrap();

    Conversation::deactivate_member_tx(&s.core.db.messages().lock(), &group, &receiver.ipk)
        .unwrap();
    let (mut tx, mut rx, _) = open_v2_request(&link.client, &receiver).await.unwrap();
    let request = Frame::Pull { file_id: file.file_id(), ranges: ranges(0, 2) };
    v2::write_frame(&mut tx, &request).await.unwrap();
    tx.finish().unwrap();
    let answer = v2::read_frame_for(&mut rx, ReadPhase::Chunk).await.unwrap();
    assert_eq!(answer, Frame::Error(ErrorCode::Unavailable));
    expect_fin(&mut rx).await.unwrap();
    serving.abort();
}

#[tokio::test]
async fn serving_and_pulling_refuse_before_any_state_or_bytes() {
    let (sender, receiver) = (identity(94), identity(95));
    let (s, r) = (device(), device());
    let group = s.group(&[sender.ipk, receiver.ipk]);
    let file = s.retain(&content(4, 3), 1024);
    s.offer(sender.ipk, group, &file);
    let elsewhere = s.retain(&content(5, 3), 1024);
    s.offer(sender.ipk, s.group(&[sender.ipk, identity(97).ipk]), &elsewhere);
    for (pulled, claimed, local, kind, why) in [
        (&file, file.total_size, identity(96), None, "another identity than the link expects"),
        (&elsewhere, elsewhere.total_size, receiver.clone(), Some(FailureKind::Unavailable),
            "a file offered to someone else"),
        (&file, 1024, receiver.clone(), Some(FailureKind::InvalidData),
            "a manifest that belies the offered size"),
    ] {
        let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
        let serving = tokio::spawn(serve_streams(s.core, link.server.clone(), sender.clone()));
        let fid = pulled.file_id();
        let lease = store::receiver_lease(&r.core.db, fid);
        let error =
            pull_live(r.core, &link.client, fid, claimed, &local, &lease).await.unwrap_err();
        assert!(kind.is_none_or(|kind| error.kind == kind), "{why}: {error:?}");
        assert!(r.partial(&fid).is_none(), "{why}: no row");
        let part = store::partial_path(&r.core.db, &fid);
        assert!(!std::path::Path::new(&part).exists(), "{why}: no bytes");
        serving.abort();
    }
    let kept = store::retention_get_tx(&s.core.db.transfers().lock(), &elsewhere.file_id());
    assert!(kept.is_some(), "still kept for its recipient");
}

#[tokio::test]
async fn group_revocation_interrupts_a_flow_controlled_v2_response() {
    let (sender, receiver) = (identity(86), identity(87));
    let s = device();
    let group = s.group(&[sender.ipk, receiver.ipk]);
    let file = s.retain(&vec![0x6d; 8 * wire::CHUNK_SIZE], wire::CHUNK_SIZE);
    s.offer(sender.ipk, group, &file);
    let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
    let serving = tokio::spawn(serve_streams(s.core, link.server.clone(), sender.clone()));

    let (mut tx, mut rx, _) = open_v2_request(&link.client, &receiver).await.unwrap();
    v2::write_frame(&mut tx, &Frame::Pull { file_id: file.file_id(), ranges: ranges(0, 8) })
        .await
        .unwrap();
    tx.finish().unwrap();
    let mut received = 0;
    loop {
        match v2::read_frame_for(&mut rx, ReadPhase::Chunk).await.unwrap() {
            Frame::Chunk { index, .. } => {
                assert_eq!(index, received);
                received += 1;
                if received == 1 {
                    let db = s.core.db.messages().lock();
                    Conversation::deactivate_member_tx(&db, &group, &receiver.ipk).unwrap();
                }
            },
            Frame::Error(ErrorCode::Unavailable) => break,
            frame => panic!("expected the revocation before completion, got {frame:?}"),
        }
    }
    assert!(
        (1..8).contains(&received),
        "chunks in flight may land, the rest of the response may not"
    );
    expect_fin(&mut rx).await.unwrap();
    serving.abort();
}

#[derive(Clone, Copy, Debug)]
enum Corruption {
    Hash,
    Length,
    Unrequested,
    OutOfOrder,
    Duplicate,
    PrematureComplete,
    CompleteCount,
    MissingComplete,
    TrailingData,
}

#[tokio::test]
async fn malformed_range_responses_are_terminal_without_claiming_bad_progress() {
    use Corruption::*;
    let (sender, receiver) = (identity(88), identity(89));
    let r = device();
    r.pair(sender.ipk);
    let cases = [
        (Hash, 0),
        (Length, 0),
        (Unrequested, 0),
        (OutOfOrder, 0),
        (Duplicate, 1),
        (PrematureComplete, 0),
        (CompleteCount, 3),
        (MissingComplete, 3),
        (TrailingData, 3),
    ];
    for (case, (corruption, verified)) in cases.into_iter().enumerate() {
        let bytes = content(100 + case as u8, 3);
        let file = manifest(&bytes, 1024);
        let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
        let (server, local, m, b) =
            (link.server.clone(), sender.clone(), file.clone(), bytes.clone());
        let serve = tokio::spawn(async move {
            describe(&server, &local, &m).await;
            let (mut s, _r, frame) = request(&server, &local).await;
            assert_eq!(frame, Frame::Pull { file_id: m.file_id(), ranges: ranges(0, 3) });
            let all = (0..3).map(|i| chunk(&m, &b, i));
            let frames: Vec<Frame> = match corruption {
                Hash | Length => {
                    let Frame::Chunk { index, mut bytes } = chunk(&m, &b, 0) else {
                        unreachable!()
                    };
                    if matches!(corruption, Hash) {
                        bytes[0] ^= 0xff
                    } else {
                        bytes.pop();
                    }
                    vec![Frame::Chunk { index, bytes }]
                },
                Unrequested => vec![Frame::Chunk { index: 7, bytes: vec![0; 1024] }],
                OutOfOrder => vec![chunk(&m, &b, 1)],
                Duplicate => vec![chunk(&m, &b, 0), chunk(&m, &b, 0)],
                PrematureComplete => vec![Frame::Complete { chunks_sent: 0 }],
                CompleteCount => all.chain([Frame::Complete { chunks_sent: 2 }]).collect(),
                MissingComplete => all.collect(),
                TrailingData => all.chain([Frame::Complete { chunks_sent: 3 }]).collect(),
            };
            // The receiver may hang up at the first bad frame; the rest is best effort.
            for frame in frames {
                let _ = v2::write_frame(&mut s, &frame).await;
            }
            if matches!(corruption, TrailingData) {
                let _ = s.write_all(&[0xee]).await;
            }
            let _ = s.finish();
        });
        let lease = store::receiver_lease(&r.core.db, file.file_id());
        let mut next = Some(link.client.clone());
        let mut attempts = 0;
        let error = drive_download_inner(
            r.core,
            file.file_id(),
            sender.ipk,
            file.total_size,
            &receiver,
            &lease,
            &[Duration::ZERO, Duration::ZERO],
            false,
            || {
                attempts += 1;
                std::future::ready(Ok(next.take().expect("invalid content must not retry")))
            },
        )
        .await
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1, "{corruption:?}");
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(failure.kind, FailureKind::InvalidData, "{corruption:?}: {failure:?}");
        let partial = r.partial(&file.file_id()).unwrap();
        assert_eq!((partial.state, partial.have), (store::FAILED, verified), "{corruption:?}");
        assert_eq!(r.verified(&file.file_id()), verified, "{corruption:?}");
        let kept = (verified as usize * 1024).min(bytes.len());
        assert_eq!(std::fs::read(&partial.path).unwrap(), bytes[..kept], "{corruption:?}");
    }
}

#[tokio::test]
async fn bad_auth_or_capabilities_fail_once_before_any_metadata() {
    let (sender, receiver) = (identity(90), identity(91));
    let r = device();
    r.pair(sender.ipk);
    for (case, bad_auth) in [true, false].into_iter().enumerate() {
        let file = manifest(&content(130 + case as u8, 2), 1024);
        let link = linked(&sender, &receiver, offered_alpns(), offered_alpns()).await;
        let (server, local) = (link.server.clone(), sender.clone());
        let serve = tokio::spawn(async move {
            let (mut s, mut r) = server.accept_stream().await.unwrap();
            if bad_auth {
                let _: wire::Auth =
                    wire::read_frame_limited(&mut r, wire::AUTH_FRAME_LIMIT).await.unwrap();
                let mut forged = local;
                forged.sig[0] ^= 0xff;
                let _ = wire::write_frame(&mut s, &forged).await;
            } else {
                auth::exchange(&server.conn, &mut s, &mut r, server.ipk, &local).await.unwrap();
                let hello = v2::read_frame_for(&mut r, ReadPhase::Hello).await.unwrap();
                assert!(matches!(hello, Frame::Hello(_)));
                let refused = v2::Hello { supported: 0, required: 0, ..v2::Hello::local() };
                let _ = v2::write_frame(&mut s, &Frame::Hello(refused)).await;
            }
            let _ = s.finish();
        });
        let lease = store::receiver_lease(&r.core.db, file.file_id());
        let mut next = Some(link.client.clone());
        let mut attempts = 0;
        let error = drive_download_inner(
            r.core,
            file.file_id(),
            sender.ipk,
            file.total_size,
            &receiver,
            &lease,
            &[Duration::ZERO, Duration::ZERO],
            false,
            || {
                attempts += 1;
                std::future::ready(Ok(next.take().expect("a rejected link is not retried")))
            },
        )
        .await
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1);
        let expected =
            if bad_auth { FailureKind::Authentication } else { FailureKind::InvalidData };
        assert_eq!(error.downcast_ref::<Failure>().unwrap().kind, expected);
        let partial = r.partial(&file.file_id()).unwrap();
        assert_eq!((partial.state, partial.have), (store::FAILED, 0));
        assert!(partial.manifest.is_none(), "the rejection precedes any metadata");
    }
}

#[tokio::test(start_paused = true)]
async fn only_transport_failures_retry_and_automatic_resumes_respect_the_outcome() {
    let r = device();
    let (peer, local) = (identity(92).ipk, identity(93));
    r.pair(peer);
    let kinds = [
        FailureKind::Transport,
        FailureKind::Authentication,
        FailureKind::InvalidData,
        FailureKind::Storage,
        FailureKind::Unavailable,
    ];
    for kind in kinds {
        let fid: [u8; 32] = rand::random();
        let lease = store::receiver_lease(&r.core.db, fid);
        let mut attempts = 0;
        let result = drive_download_inner(
            r.core,
            fid,
            peer,
            100,
            &local,
            &lease,
            &RETRY_DELAYS,
            false,
            || {
                attempts += 1;
                std::future::ready(Err(Failure::new(kind, anyhow::anyhow!("injected"))))
            },
        )
        .await;
        drop(lease);
        let held = kind == FailureKind::Transport;
        assert_eq!(attempts, if held { 1 + RETRY_DELAYS.len() } else { 1 }, "{kind:?}");
        assert_eq!(result.is_ok(), held, "{kind:?}");
        let state = if held { store::HELD } else { store::FAILED };
        assert_eq!(r.partial(&fid).unwrap().state, state, "{kind:?}");
        if held {
            assert!(store::retry_after_tx(&r.core.db.transfers().lock(), &fid) > now_secs());
        }
        // An automatic resume returns before any lookup: the cooldown holds a transient
        // failure, and nothing revives a terminal one.
        for trigger in [DownloadTrigger::Reconnect, DownloadTrigger::WakeResponse] {
            download_with_policy(r.core, fid, trigger).await.unwrap();
            assert_eq!(r.partial(&fid).unwrap().state, state, "{kind:?}");
        }
    }

    let fid: [u8; 32] = rand::random();
    let lease = store::receiver_lease(&r.core.db, fid);
    set_state(r.core, &fid, peer, store::HELD, &lease).unwrap();
    drop(lease);
    assert!(store::claim_wake_tx(&mut r.core.db.transfers().lock(), &fid, now_secs(), 60).unwrap());
    assert!(DOWNLOADING.lock().insert(fid));
    let writer = PullGuard(fid);
    download_with_policy(r.core, fid, DownloadTrigger::WakeResponse).await.unwrap();
    let claimed = store::claim_ready_retry_tx(&r.core.db.transfers().lock(), &fid).unwrap();
    assert!(claimed, "a second download of a file being pulled spends nothing");
    drop(writer);
}
