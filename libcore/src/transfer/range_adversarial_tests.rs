//! Hostile providers over the real negotiated QUIC stream. These tests exercise
//! validation, retry classification and cancellation through the production
//! receiver, while using tiny manifest chunks to keep the fixtures small.

use super::range_tests::*;
use super::*;
use crate::p2p::{PeerLink, protocol};
use v2::{ErrorCode, Frame};

async fn describe(link: &PeerLink, local: &wire::Auth, manifest: &wire::Manifest) {
    let (mut send, _recv, frame) = request(link, local).await;
    assert_eq!(frame, Frame::Describe { file_id: manifest.file_id() });
    v2::write_frame(&mut send, &Frame::Manifest(manifest.clone())).await.unwrap();
    v2::write_frame(&mut send, &Frame::Complete { chunks_sent: 0 }).await.unwrap();
    send.finish().unwrap();
}

fn chunk(manifest: &wire::Manifest, bytes: &[u8], index: u32) -> Frame {
    let start = index as usize * manifest.chunk_size as usize;
    let end = (start + manifest.chunk_size as usize).min(bytes.len());
    Frame::Chunk { index, bytes: bytes[start..end].to_vec() }
}

async fn wait_for_prefix(file_id: [u8; 32], have: u32) {
    timeout(Duration::from_secs(3), async {
        loop {
            if store::partial_get(&file_id).is_some_and(|p| p.have == have) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
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
    let sender = identity(140);
    let receiver = identity(141);
    for (case_index, corruption) in [
        Corruption::Hash,
        Corruption::Length,
        Corruption::Unrequested,
        Corruption::OutOfOrder,
        Corruption::Duplicate,
        Corruption::PrematureComplete,
        Corruption::CompleteCount,
        Corruption::MissingComplete,
        Corruption::TrailingData,
    ]
    .into_iter()
    .enumerate()
    {
        let (bytes, manifest) = fixture(100 + case_index as u8, 3);
        let file_id = manifest.file_id();
        let size = manifest.total_size;
        let (server, client, _server_endpoint, _client_endpoint) =
            linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
        assert_eq!(client.protocol().unwrap(), protocol::AttachmentProtocol::V2);
        let server_task_link = server.clone();
        let local = sender.clone();
        let fixture_bytes = bytes.clone();
        let fixture_manifest = manifest.clone();
        let serve = tokio::spawn(async move {
            describe(&server_task_link, &local, &fixture_manifest).await;
            let (mut send, _recv, frame) = request(&server_task_link, &local).await;
            assert_eq!(
                frame,
                Frame::Pull { file_id, ranges: vec![ranges::ChunkRange { start: 0, end: 3 }] }
            );
            let mut frames = Vec::new();
            match corruption {
                Corruption::Hash => {
                    let mut bad = chunk(&fixture_manifest, &fixture_bytes, 0);
                    if let Frame::Chunk { bytes, .. } = &mut bad {
                        bytes[0] ^= 0xff;
                    }
                    frames.push(bad);
                },
                Corruption::Length => {
                    let mut bad = chunk(&fixture_manifest, &fixture_bytes, 0);
                    if let Frame::Chunk { bytes, .. } = &mut bad {
                        bytes.pop();
                    }
                    frames.push(bad);
                },
                Corruption::Unrequested => {
                    frames.push(Frame::Chunk { index: 7, bytes: vec![0; 1024] })
                },
                Corruption::OutOfOrder => frames.push(chunk(&fixture_manifest, &fixture_bytes, 1)),
                Corruption::Duplicate => {
                    frames.push(chunk(&fixture_manifest, &fixture_bytes, 0));
                    frames.push(chunk(&fixture_manifest, &fixture_bytes, 0));
                },
                Corruption::PrematureComplete => frames.push(Frame::Complete { chunks_sent: 0 }),
                Corruption::CompleteCount
                | Corruption::MissingComplete
                | Corruption::TrailingData => {
                    for index in 0..3 {
                        frames.push(chunk(&fixture_manifest, &fixture_bytes, index));
                    }
                    if !matches!(corruption, Corruption::MissingComplete) {
                        frames.push(Frame::Complete {
                            chunks_sent: if matches!(corruption, Corruption::CompleteCount) {
                                2
                            } else {
                                3
                            },
                        });
                    }
                },
            }
            for frame in frames {
                v2::write_frame(&mut send, &frame).await.unwrap();
            }
            if matches!(corruption, Corruption::TrailingData) {
                send.write_all(&[0xee]).await.unwrap();
            }
            send.finish().unwrap();
        });
        let lease = store::receiver_lease(file_id);
        let mut next = Some(client);
        let mut attempts = 0;
        let error = timeout(
            Duration::from_secs(5),
            drive_download(
                file_id,
                sender.ipk,
                size,
                &receiver,
                &lease,
                &[Duration::ZERO, Duration::ZERO],
                || {
                    attempts += 1;
                    let link = next.take().expect("invalid v2 content must not retry or fall back");
                    async move { Ok(link) }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1, "{corruption:?}");
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(failure.kind, FailureKind::InvalidData, "{corruption:?}: {failure:?}");
        let expected_have = match corruption {
            Corruption::Duplicate => 1,
            Corruption::CompleteCount | Corruption::MissingComplete | Corruption::TrailingData => 3,
            _ => 0,
        };
        let partial = store::partial_get(&file_id).unwrap();
        assert_eq!(partial.have, expected_have, "{corruption:?}");
        assert_eq!(store::verified_count(&partial), expected_have, "{corruption:?}");
        assert_eq!(partial.state, store::FAILED, "{corruption:?} must not complete the file");
        let saved = std::fs::read(&partial.path).unwrap();
        assert_eq!(
            saved,
            bytes[..(expected_have as usize * manifest.chunk_size as usize).min(bytes.len())]
        );
        store::forget_partial(&file_id);
    }
}

#[tokio::test]
async fn typed_midstream_errors_preserve_only_verified_bytes_and_do_not_retry() {
    let sender = identity(142);
    let receiver = identity(143);
    for (case_index, (code, expected_kind)) in [
        (ErrorCode::Unavailable, FailureKind::Unavailable),
        (ErrorCode::InvalidRequest, FailureKind::InvalidData),
        (ErrorCode::Unsupported, FailureKind::InvalidData),
        (ErrorCode::Storage, FailureKind::Storage),
    ]
    .into_iter()
    .enumerate()
    {
        let (bytes, manifest) = fixture(120 + case_index as u8, 2);
        let file_id = manifest.file_id();
        let size = manifest.total_size;
        let (server, client, _server_endpoint, _client_endpoint) =
            linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
        let task_link = server.clone();
        let local = sender.clone();
        let fixture_bytes = bytes.clone();
        let fixture_manifest = manifest.clone();
        let serve = tokio::spawn(async move {
            describe(&task_link, &local, &fixture_manifest).await;
            let (mut send, _recv, frame) = request(&task_link, &local).await;
            assert!(matches!(frame, Frame::Pull { .. }));
            v2::write_frame(&mut send, &chunk(&fixture_manifest, &fixture_bytes, 0)).await.unwrap();
            v2::write_frame(&mut send, &Frame::Error(code)).await.unwrap();
            send.finish().unwrap();
        });
        let lease = store::receiver_lease(file_id);
        let mut next = Some(client);
        let mut attempts = 0;
        let error = timeout(
            Duration::from_secs(5),
            drive_download(
                file_id,
                sender.ipk,
                size,
                &receiver,
                &lease,
                &[Duration::ZERO, Duration::ZERO],
                || {
                    attempts += 1;
                    let link = next.take().expect("terminal remote error must not retry");
                    async move { Ok(link) }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1);
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.source.downcast_ref::<ErrorCode>(), Some(&code));
        let partial = store::partial_get(&file_id).unwrap();
        assert_eq!(partial.state, store::FAILED);
        assert_eq!(partial.have, 1);
        assert_eq!(store::verified_count(&partial), 1);
        assert_eq!(std::fs::read(&partial.path).unwrap(), bytes[..manifest.chunk_size as usize]);
        store::forget_partial(&file_id);
    }
}

#[tokio::test]
async fn busy_retries_finitely_and_preserves_completed_chunks_across_new_streams() {
    let sender = identity(144);
    let receiver = identity(145);
    let (bytes, manifest) = fixture(124, 2);
    let file_id = manifest.file_id();
    let size = manifest.total_size;
    let mut links = std::collections::VecDeque::new();
    let mut alive = Vec::new();
    let mut serving = Vec::new();
    for attempt in 0..3 {
        let (server, client, server_endpoint, client_endpoint) =
            linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
        assert_eq!(client.protocol().unwrap(), protocol::AttachmentProtocol::V2);
        let task_link = server.clone();
        let local = sender.clone();
        let fixture_bytes = bytes.clone();
        let fixture_manifest = manifest.clone();
        serving.push(tokio::spawn(async move {
            describe(&task_link, &local, &fixture_manifest).await;
            let (mut send, _recv, frame) = request(&task_link, &local).await;
            assert_eq!(
                frame,
                Frame::Pull {
                    file_id,
                    ranges: vec![ranges::ChunkRange {
                        start: if attempt == 0 { 0 } else { 1 },
                        end: 2
                    }],
                }
            );
            if attempt == 0 {
                v2::write_frame(&mut send, &chunk(&fixture_manifest, &fixture_bytes, 0))
                    .await
                    .unwrap();
            }
            v2::write_frame(&mut send, &Frame::Error(ErrorCode::Busy)).await.unwrap();
            send.finish().unwrap();
        }));
        // Mirror the production connection cache: the request worker is not
        // the final owner of either shared connection.
        alive.push((server, client.clone(), server_endpoint, client_endpoint));
        links.push_back(client);
    }
    let lease = store::receiver_lease(file_id);
    let mut attempts = 0;
    let held = timeout(
        Duration::from_secs(5),
        drive_download(
            file_id,
            sender.ipk,
            size,
            &receiver,
            &lease,
            &[Duration::ZERO, Duration::ZERO],
            || {
                attempts += 1;
                let link = links.pop_front().expect("busy must obey the finite retry budget");
                async move { Ok(link) }
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(held);
    assert_eq!(attempts, 3);
    assert!(links.is_empty());
    for (server, _, _, _) in &alive {
        assert!(
            server.conn.close_reason().is_none(),
            "Busy must not close other transfers' shared connection"
        );
    }
    for serve in serving {
        serve.await.unwrap();
    }
    let partial = store::partial_get(&file_id).unwrap();
    assert_eq!(partial.have, 1);
    assert_eq!(store::verified_count(&partial), 1);
    assert_eq!(partial.state, store::HELD);
    assert!(store::retry_after(&file_id) > crate::utils::systime().as_secs());
    assert_eq!(std::fs::read(&partial.path).unwrap(), bytes[..manifest.chunk_size as usize]);
    store::forget_partial(&file_id);
}

#[tokio::test]
async fn negotiated_v2_never_retries_legacy_after_bad_auth_or_capabilities() {
    let sender = identity(146);
    let receiver = identity(147);
    for (id, bad_auth) in [(125, true), (127, false)] {
        let (_bytes, manifest) = fixture(id, 2);
        let file_id = manifest.file_id();
        let (server, client, _server_endpoint, _client_endpoint) =
            linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
        assert_eq!(client.protocol().unwrap(), protocol::AttachmentProtocol::V2);
        let task_link = server.clone();
        let local = sender.clone();
        let serve = tokio::spawn(async move {
            let (mut send, mut recv) = task_link.accept_stream().await.unwrap();
            if bad_auth {
                let _peer: wire::Auth =
                    wire::read_frame_limited(&mut recv, wire::AUTH_FRAME_LIMIT).await.unwrap();
                let mut forged = local;
                forged.sig[0] ^= 0xff;
                wire::write_frame(&mut send, &forged).await.unwrap();
            } else {
                auth::exchange(&task_link.conn, &mut send, &mut recv, task_link.ipk, &local)
                    .await
                    .unwrap();
                assert!(matches!(v2::read_frame(&mut recv).await.unwrap(), Frame::Hello(_)));
                v2::write_frame(
                    &mut send,
                    &Frame::Hello(v2::Hello { supported: 0, required: 0, ..v2::Hello::local() }),
                )
                .await
                .unwrap();
            }
            send.finish().unwrap();
        });
        let lease = store::receiver_lease(file_id);
        let mut next = Some(client);
        let mut attempts = 0;
        let error = timeout(
            Duration::from_secs(5),
            drive_download(
                file_id,
                sender.ipk,
                manifest.total_size,
                &receiver,
                &lease,
                &[Duration::ZERO, Duration::ZERO],
                || {
                    attempts += 1;
                    let link = next.take().expect("selected v2 must not downgrade after rejection");
                    async move { Ok(link) }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().kind,
            if bad_auth { FailureKind::Authentication } else { FailureKind::InvalidData }
        );
        let partial = store::partial_get(&file_id).unwrap();
        assert_eq!(partial.state, store::FAILED);
        assert_eq!(partial.have, 0);
        assert!(partial.manifest.is_none(), "auth/capability rejection precedes metadata");
        store::forget_partial(&file_id);
    }
}

#[tokio::test]
async fn deletion_cancels_a_stalled_range_and_stops_the_remote_stream() {
    let sender = identity(148);
    let receiver = identity(149);
    let (bytes, manifest) = fixture(126, 3);
    let file_id = manifest.file_id();
    let size = manifest.total_size;
    let (server, client, _server_endpoint, _client_endpoint) =
        linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
    let task_link = server.clone();
    let local = sender.clone();
    let serve = tokio::spawn(async move {
        describe(&task_link, &local, &manifest).await;
        let (mut send, _recv, frame) = request(&task_link, &local).await;
        assert!(matches!(frame, Frame::Pull { .. }));
        v2::write_frame(&mut send, &chunk(&manifest, &bytes, 0)).await.unwrap();
        // Keep the stream open. Dropping the receiver future must produce
        // STOP_SENDING without waiting for the normal idle timeout.
        timeout(Duration::from_secs(3), send.stopped()).await.unwrap().unwrap()
    });
    let lease = store::receiver_lease(file_id);
    let cancelled = lease.cancel.clone();
    let task_client = client.clone();
    let downloading = tokio::spawn(async move {
        pull_live(&task_client, file_id, size, &receiver, &lease, CHUNK_TIMEOUT).await
    });
    wait_for_prefix(file_id, 1).await;
    let path = store::partial_get(&file_id).unwrap().path;
    assert!(std::path::Path::new(&path).exists());
    store::forget_partial(&file_id);
    assert!(cancelled.is_cancelled());
    let error = timeout(Duration::from_secs(1), downloading).await.unwrap().unwrap().unwrap_err();
    assert_eq!(error.kind, FailureKind::Cancelled);
    assert!(serve.await.unwrap().is_some(), "server observed STOP_SENDING");
    assert!(store::partial_get(&file_id).is_none());
    assert!(!std::path::Path::new(&path).exists());
    assert!(
        client.conn.close_reason().is_none(),
        "cancelling one range leaves the connection usable"
    );
}

#[tokio::test]
async fn typed_hello_errors_are_terminal_before_metadata_or_retry() {
    let sender = identity(146);
    let receiver = identity(147);
    for (id, code, kind) in [
        (128, ErrorCode::Storage, FailureKind::Storage),
        (129, ErrorCode::Unavailable, FailureKind::Unavailable),
        (130, ErrorCode::Unsupported, FailureKind::InvalidData),
    ] {
        let (_bytes, manifest) = fixture(id, 1);
        let file_id = manifest.file_id();
        let (server, client, _server_endpoint, _client_endpoint) =
            linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
        let task_link = server.clone();
        let local = sender.clone();
        let serve = tokio::spawn(async move {
            let (mut send, mut recv) = task_link.accept_stream().await.unwrap();
            auth::exchange(&task_link.conn, &mut send, &mut recv, task_link.ipk, &local)
                .await
                .unwrap();
            assert!(matches!(v2::read_frame(&mut recv).await.unwrap(), Frame::Hello(_)));
            v2::write_frame(&mut send, &Frame::Error(code)).await.unwrap();
            send.finish().unwrap();
        });
        let lease = store::receiver_lease(file_id);
        let mut next = Some(client);
        let mut attempts = 0;
        let error = timeout(
            Duration::from_secs(5),
            drive_download(
                file_id,
                sender.ipk,
                manifest.total_size,
                &receiver,
                &lease,
                &[Duration::ZERO, Duration::ZERO],
                || {
                    attempts += 1;
                    let link = next.take().expect("terminal Hello errors must not retry");
                    async move { Ok(link) }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        serve.await.unwrap();
        assert_eq!(attempts, 1);
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.source.downcast_ref::<ErrorCode>(), Some(&code));
        let partial = store::partial_get(&file_id).unwrap();
        assert_eq!(partial.state, store::FAILED);
        assert!(partial.manifest.is_none());
        store::forget_partial(&file_id);
    }
}

#[tokio::test]
async fn dropping_an_unfinished_response_resets_the_stream_and_allows_recovery() {
    let sender = identity(148);
    let receiver = identity(149);
    let (bytes, manifest) = fixture(131, 2);
    let file_id = manifest.file_id();
    let size = manifest.total_size;
    let (first_server, first_client, _first_a, _first_b) =
        linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
    let (second_server, second_client, _second_a, _second_b) =
        linked(&sender, &receiver, protocol::offered_alpns(), protocol::offered_alpns()).await;
    let task_link = first_server.clone();
    let local = sender.clone();
    let first = tokio::spawn(async move {
        let (send, _recv, frame) = request(&task_link, &local).await;
        assert_eq!(frame, Frame::Describe { file_id });
        let mut send = TransferSend::new(send);
        // Simulate cancellation during the fixed header. Raw SendStream drop
        // sends FIN; the production owner must reset this unfinished response.
        send.write_all(&[3, 0, 1]).await.unwrap();
        drop(send);
    });
    let task_link = second_server.clone();
    let local = sender.clone();
    let expected_bytes = bytes.clone();
    let complete = tokio::spawn(async move {
        describe(&task_link, &local, &manifest).await;
        let (mut send, _recv, frame) = request(&task_link, &local).await;
        assert_eq!(
            frame,
            Frame::Pull { file_id, ranges: vec![ranges::ChunkRange { start: 0, end: 2 }] }
        );
        for index in 0..2 {
            v2::write_frame(&mut send, &chunk(&manifest, &expected_bytes, index)).await.unwrap();
        }
        v2::write_frame(&mut send, &Frame::Complete { chunks_sent: 2 }).await.unwrap();
        send.finish().unwrap();
    });
    let lease = store::receiver_lease(file_id);
    let mut links = std::collections::VecDeque::from([first_client, second_client]);
    let mut attempts = 0;
    let held = timeout(
        Duration::from_secs(5),
        drive_download(file_id, sender.ipk, size, &receiver, &lease, &[Duration::ZERO], || {
            attempts += 1;
            let link = links.pop_front().expect("exactly one recovery after reset");
            async move { Ok(link) }
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!held);
    assert_eq!(attempts, 2, "reset is recoverable, unlike malformed clean FIN");
    first.await.unwrap();
    complete.await.unwrap();
    let partial = store::partial_get(&file_id).unwrap();
    assert_eq!(partial.state, store::DONE);
    assert_eq!(std::fs::read(&partial.path).unwrap(), bytes);
    store::forget_partial(&file_id);
}
