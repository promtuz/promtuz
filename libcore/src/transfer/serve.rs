use common::utils::now_secs;
use tokio::sync::Semaphore;

use super::CHUNK_DEADLINE;
use super::CHUNK_TIMEOUT;
use super::CONTROL_TIMEOUT;
use super::Failure;
use super::FailureKind;
use super::TransferSend;
use super::auth;
use super::bounded;
use super::expect_fin;
use super::ranges;
use super::report_failure;
use super::sharing;
use super::store;
use super::v2;
use super::wire;
use crate::p2p::diagnostics;
use crate::state::Core;

const SERVES_PER_LINK: usize = 4;
static SERVING: Semaphore = Semaphore::const_new(16);
static HELPING: Semaphore = Semaphore::const_new(2);

async fn write_chunk(s: &mut quinn::SendStream, buf: &[u8]) -> Result<(), Failure> {
    let deadline = tokio::time::Instant::now() + CHUNK_DEADLINE;
    let mut written = 0;
    while written < buf.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(
                FailureKind::Transport,
                anyhow::anyhow!("chunk progress deadline exceeded"),
            ));
        }
        written +=
            bounded(CHUNK_TIMEOUT.min(remaining), async { Ok(s.write(&buf[written..]).await?) })
                .await?;
    }
    Ok(())
}

/// Retention is keyed by content hash alone, so the outgoing message row scopes a pull to whom
/// the file was sent: in a group, every active member of that conversation.
pub(super) fn offered_to(c: &Core, file_id: &[u8; 32], peer: &[u8; 32], me: &[u8; 32]) -> bool {
    let paired = crate::data::contact::Contact::is_paired_tx(&c.db.contacts().lock(), peer);
    let db = c.db.messages().lock();
    db.query_row(
        "SELECT 1 FROM message_media mm
           JOIN messages m
             ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
           JOIN conversations c ON c.id = mm.conversation_id
           JOIN conversation_members cm ON cm.conversation_id = mm.conversation_id
           LEFT JOIN conversation_members mine
             ON mine.conversation_id = mm.conversation_id AND mine.member_ipk = ?3
          WHERE mm.file_id = ?1 AND cm.member_ipk = ?2 AND cm.active = 1 AND m.outgoing = 1
            AND m.deleted = 0
            AND ((c.kind = ?4 AND mine.active = 1) OR (c.kind = ?5 AND ?6))
          LIMIT 1",
        rusqlite::params![
            file_id.as_slice(),
            peer.as_slice(),
            me.as_slice(),
            crate::data::conversation::KIND_GROUP,
            crate::data::conversation::KIND_DIRECT,
            paired,
        ],
        |_| Ok(()),
    )
    .is_ok()
}

pub(super) async fn serve_streams(c: &'static Core, link: crate::p2p::PeerLink, local: wire::Auth) {
    let mut streams = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = streams.join_next(), if !streams.is_empty() => {},
            accepted = link.accept_stream(), if streams.len() < SERVES_PER_LINK => {
                let Ok((mut s, mut r)) = accepted else { break };
                // Never queue work: a busy stream resets, and its caller retries within its budget.
                let Ok(slot) = SERVING.try_acquire() else {
                    let _ = s.reset(0u32.into());
                    let _ = r.stop(0u32.into());
                    continue;
                };
                let link = link.clone();
                let local = local.clone();
                streams.spawn(async move {
                    let _slot = slot;
                    if let Err(e) = serve_stream(c, &link, &local, s, r).await {
                        if e.kind == FailureKind::Authentication {
                            link.conn.close(0u32.into(), b"transfer authentication failed");
                        }
                        report_failure(e.kind);
                        log::debug!("transfer: serve ended: {e}");
                    }
                });
            },
        }
    }
}

async fn serve_stream(
    c: &Core, link: &crate::p2p::PeerLink, local: &wire::Auth, s: quinn::SendStream,
    mut r: quinn::RecvStream,
) -> Result<(), Failure> {
    let mut s = TransferSend::new(s);
    bounded(CONTROL_TIMEOUT, auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local)).await?;
    if link.protocol().map_err(|e| Failure::new(FailureKind::InvalidData, e))?
        == crate::p2p::protocol::AttachmentProtocol::V2
    {
        return serve_v2(c, link, local, &mut s, &mut r).await;
    }
    let pull: wire::Pull =
        bounded(CONTROL_TIMEOUT, wire::read_frame_limited(&mut r, wire::PULL_FRAME_LIMIT)).await?;
    let now = now_secs();
    let retained = store::retention_get_tx(&c.db.transfers().lock(), &pull.file_id)
        .filter(|r| r.expires_at > now);
    let retained = retained.filter(|_| offered_to(c, &pull.file_id, &link.ipk, &local.ipk));
    let Some(ret) = retained else {
        bounded(CONTROL_TIMEOUT, wire::write_frame(&mut s, &wire::ServeResp::Gone)).await?;
        let _ = s.finish();
        return Ok(());
    };
    let manifest: wire::Manifest = postcard::from_bytes(&ret.manifest).map_err(Failure::storage)?;
    if manifest.chunk_size == 0
        || manifest.chunk_size as usize > wire::CHUNK_SIZE
        || manifest.file_id() != pull.file_id
        || pull.have as usize > manifest.chunks.len()
        || manifest.chunks.len() as u64 != manifest.total_size.div_ceil(manifest.chunk_size as u64)
    {
        return Err(Failure::new(
            FailureKind::InvalidData,
            anyhow::anyhow!("invalid retained manifest or prefix"),
        ));
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(&ret.path) {
        Ok(f) => f,
        Err(_) => {
            bounded(CONTROL_TIMEOUT, wire::write_frame(&mut s, &wire::ServeResp::Gone)).await?;
            let _ = s.finish();
            return Ok(());
        },
    };
    f.seek(SeekFrom::Start(pull.have as u64 * manifest.chunk_size as u64))
        .map_err(Failure::storage)?;
    bounded(
        CONTROL_TIMEOUT,
        wire::write_frame(&mut s, &wire::ServeResp::Manifest(manifest.clone())),
    )
    .await?;
    let mut buf = vec![0u8; manifest.chunk_size as usize];
    for idx in pull.have as usize..manifest.chunks.len() {
        // Revocation/deletion is rechecked between chunks. Bytes already sent
        // cannot be recalled, but an open fd is not unlimited serving consent.
        if !offered_to(c, &pull.file_id, &link.ipk, &local.ipk)
            || store::retention_get_tx(&c.db.transfers().lock(), &pull.file_id)
                .is_none_or(|r| r.expires_at <= now_secs())
        {
            return Err(Failure::new(
                FailureKind::Unavailable,
                anyhow::anyhow!("attachment no longer available"),
            ));
        }
        let count = (manifest.total_size - idx as u64 * manifest.chunk_size as u64)
            .min(manifest.chunk_size as u64) as usize;
        f.read_exact(&mut buf[..count]).map_err(Failure::storage)?;
        write_chunk(&mut s, &buf[..count]).await?;
        diagnostics::sent_content(count as u64);
    }
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    Ok(())
}

async fn v2_error(s: &mut TransferSend, code: v2::ErrorCode) -> Result<(), Failure> {
    bounded(CONTROL_TIMEOUT, v2::write_frame(s, &v2::Frame::Error(code))).await?;
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    Ok(())
}

/// Each stream carries one Describe or Pull, with its own cancellation scope.
async fn serve_v2(
    c: &Core, link: &crate::p2p::PeerLink, local: &wire::Auth, s: &mut TransferSend,
    r: &mut quinn::RecvStream,
) -> Result<(), Failure> {
    let limits = bounded(CONTROL_TIMEOUT, v2::exchange_hello(s, r)).await?;
    let request = bounded(CONTROL_TIMEOUT, v2::read_frame_for(r, v2::ReadPhase::Request)).await?;
    expect_fin(r).await?;
    let (file_id, requested, grant) = match request {
        v2::Frame::Describe { file_id } => (file_id, None, None),
        v2::Frame::Pull { file_id, ranges } => (file_id, Some(ranges), None),
        v2::Frame::DescribeShared { file_id, grant } if limits.sharing => (file_id, None, Some(grant)),
        v2::Frame::PullShared { file_id, grant, ranges } if limits.sharing => (file_id, Some(ranges), Some(grant)),
        v2::Frame::DescribeShared { .. } | v2::Frame::PullShared { .. } => return v2_error(s, v2::ErrorCode::Unsupported).await,
        _ => return v2_error(s, v2::ErrorCode::InvalidRequest).await,
    };
    if grant.is_some() {
        let Some(scope) = sharing::serving_scope() else { return v2_error(s, v2::ErrorCode::Unavailable).await };
        let Ok(_slot) = HELPING.try_acquire() else { return v2_error(s, v2::ErrorCode::Busy).await };
        tokio::select! {
            biased;
            _ = scope.cancelled() => Err(Failure::new(FailureKind::Unavailable, anyhow::anyhow!("recipient uploads suspended"))),
            result = serve_v2_file(c, link, local, s, limits, file_id, requested, grant, Some(&scope)) => result,
        }
    } else {
        serve_v2_file(c, link, local, s, limits, file_id, requested, None, None).await
    }
}

fn can_serve(
    c: &Core, file_id: &[u8; 32], peer: &[u8; 32], me: &[u8; 32], grant: Option<&[u8; 32]>,
) -> bool {
    match grant {
        Some(id) => sharing::permitted(c, id, file_id, me, peer).is_some(),
        None => {
            offered_to(c, file_id, peer, me)
                && store::retention_get_tx(&c.db.transfers().lock(), file_id)
                    .is_some_and(|ret| ret.expires_at > now_secs())
        },
    }
}

async fn serve_v2_file(
    c: &Core, link: &crate::p2p::PeerLink, local: &wire::Auth, s: &mut TransferSend,
    limits: v2::Limits, file_id: [u8; 32], requested: Option<Vec<ranges::ChunkRange>>, grant: Option<[u8; 32]>,
    helper_cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), Failure> {
    if helper_cancel.is_some_and(|t| t.is_cancelled()) || !can_serve(c, &file_id, &link.ipk, &local.ipk, grant.as_ref()) {
        return v2_error(s, v2::ErrorCode::Unavailable).await;
    }
    let retained = if grant.is_some() {
        sharing::completed_copy(c, &file_id)
    } else {
        store::retention_get_tx(&c.db.transfers().lock(), &file_id)
    };
    let Some(retained) = retained else { return v2_error(s, v2::ErrorCode::Unavailable).await };
    let manifest: wire::Manifest = match postcard::from_bytes(&retained.manifest) {
        Ok(manifest) => manifest,
        Err(_) => return v2_error(s, v2::ErrorCode::Storage).await,
    };
    if manifest.file_id() != file_id
        || manifest.chunk_size == 0
        || manifest.chunk_size as usize > wire::CHUNK_SIZE
        || manifest.chunks.len() as u64 != manifest.total_size.div_ceil(manifest.chunk_size as u64)
        || manifest.total_size != retained.size
    {
        return v2_error(s, v2::ErrorCode::Storage).await;
    }
    let Some(requested) = requested else {
        bounded(CONTROL_TIMEOUT, v2::write_frame(s, &v2::Frame::Manifest(manifest))).await?;
        bounded(CONTROL_TIMEOUT, v2::write_frame(s, &v2::Frame::Complete { chunks_sent: 0 }))
            .await?;
        s.finish().map_err(|e| Failure::wire(e.into()))?;
        return Ok(());
    };
    let count = match v2::validate_ranges(&requested, &manifest, limits) {
        Ok(count) => count,
        Err(_) => return v2_error(s, v2::ErrorCode::InvalidRequest).await,
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(&retained.path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return v2_error(s, v2::ErrorCode::Unavailable).await;
        },
        Err(_) => return v2_error(s, v2::ErrorCode::Storage).await,
    };
    for range in requested {
        for index in range.start..range.end {
            if helper_cancel.is_some_and(|t| t.is_cancelled()) || !can_serve(c, &file_id, &link.ipk, &local.ipk, grant.as_ref()) {
                return v2_error(s, v2::ErrorCode::Unavailable).await;
            }
            let offset = index as u64 * manifest.chunk_size as u64;
            let len = (manifest.total_size - offset).min(manifest.chunk_size as u64) as usize;
            let mut bytes = vec![0; len];
            if file.seek(SeekFrom::Start(offset)).is_err() || file.read_exact(&mut bytes).is_err() {
                return v2_error(s, v2::ErrorCode::Storage).await;
            }
            // Detect an edited/truncated source before advertising bad data.
            if blake3::hash(&bytes).as_bytes() != &manifest.chunks[index as usize] {
                return v2_error(s, v2::ErrorCode::Storage).await;
            }
            bounded(CHUNK_DEADLINE, v2::write_frame(s, &v2::Frame::Chunk { index, bytes })).await?;
            diagnostics::sent_content(len as u64);
        }
    }
    bounded(CONTROL_TIMEOUT, v2::write_frame(s, &v2::Frame::Complete { chunks_sent: count }))
        .await?;
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    Ok(())
}
