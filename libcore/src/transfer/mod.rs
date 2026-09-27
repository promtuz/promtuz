//! P2P attachment transfer: chunked-manifest protocol for files too big for
//! the inline `Image` message (>256KB), carried over a direct link from
//! [`crate::p2p`] rather than the store-and-forward relay.

use std::collections::HashSet;
use std::time::Duration;

use crate::p2p::diagnostics::{self, Event};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

pub mod auth;
pub(crate) mod ranges;
pub mod store;
pub(crate) mod v2;
pub mod wire;

/// Auto-download ceiling: bigger offers wait for a user tap even on wifi.
pub const AUTO_MAX: u64 = 5 * 1024 * 1024;

/// Abandoned receiver partials older than this are reaped by [`gc`].
const DEAD_PARTIAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Don't re-wake a still-offline sender on every reconnect: only re-send the
/// reverse-wake if the held partial hasn't been poked within this window.
const WAKE_BACKOFF_SECS: u64 = 60;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(8);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_DEADLINE: Duration = Duration::from_secs(5 * 60);
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
const RETRY_COOLDOWN_SECS: u64 = 60;
const SERVES_PER_LINK: usize = 4;
static SERVING: Semaphore = Semaphore::const_new(16);
static PULLING: Semaphore = Semaphore::const_new(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    Transport,
    Authentication,
    InvalidData,
    Storage,
    Unavailable,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {source}")]
struct Failure {
    kind: FailureKind,
    #[source]
    source: anyhow::Error,
}
impl Failure {
    fn new(kind: FailureKind, source: impl Into<anyhow::Error>) -> Self {
        Self { kind, source: source.into() }
    }
    fn wire(e: anyhow::Error) -> Self {
        if let Some(code) = e.downcast_ref::<v2::ErrorCode>() {
            return remote_failure(*code);
        }
        let kind = if e.is::<auth::AuthenticationFailed>() {
            FailureKind::Authentication
        } else if e.is::<wire::InvalidFrame>() {
            FailureKind::InvalidData
        } else {
            FailureKind::Transport
        };
        Self::new(kind, e)
    }
    fn storage(e: impl Into<anyhow::Error>) -> Self {
        let e = e.into();
        let kind =
            if e.is::<store::Cancelled>() { FailureKind::Cancelled } else { FailureKind::Storage };
        Self::new(kind, e)
    }
}

/// Quinn normally finishes a dropped sending half. An interrupted frame must
/// reset instead, so a transport stall is not mistaken for malformed clean EOF.
struct TransferSend {
    stream: quinn::SendStream,
    finished: bool,
}
impl TransferSend {
    fn new(stream: quinn::SendStream) -> Self {
        Self { stream, finished: false }
    }
    fn finish(&mut self) -> Result<(), quinn::ClosedStream> {
        self.stream.finish()?;
        self.finished = true;
        Ok(())
    }
}
impl std::ops::Deref for TransferSend {
    type Target = quinn::SendStream;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl std::ops::DerefMut for TransferSend {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}
impl Drop for TransferSend {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.stream.reset(0u32.into());
        }
    }
}

async fn bounded<T>(
    limit: Duration, work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> Result<T, Failure> {
    timeout(limit, work)
        .await
        .map_err(|_| Failure::new(FailureKind::Transport, anyhow::anyhow!("transfer stalled")))?
        .map_err(Failure::wire)
}

/// Count stream progress rather than requiring an entire 256 KiB chunk within
/// the idle timeout. A slow but healthy path still advances; a drip-fed chunk
/// has a separate absolute ceiling so it cannot hold a worker indefinitely.
async fn read_chunk(
    r: &mut quinn::RecvStream, buf: &mut [u8], idle: Duration,
) -> Result<(), Failure> {
    let deadline = tokio::time::Instant::now() + CHUNK_DEADLINE;
    let mut filled = 0;
    while filled < buf.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(
                FailureKind::Transport,
                anyhow::anyhow!("chunk progress deadline exceeded"),
            ));
        }
        let n = bounded(idle.min(remaining), async {
            r.read(&mut buf[filled..])
                .await?
                .ok_or_else(|| anyhow::anyhow!("attachment stream ended before chunk completed"))
        })
        .await?;
        filled += n;
    }
    Ok(())
}

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

fn report_failure(kind: FailureKind) {
    let event = match kind {
        FailureKind::Authentication => Event::AuthenticationFailed,
        FailureKind::InvalidData => Event::ValidationFailed,
        FailureKind::Storage => Event::StorageFailed,
        FailureKind::Unavailable => Event::Unavailable,
        FailureKind::Transport | FailureKind::Cancelled => return,
    };
    diagnostics::record(event);
}

/// Periodic housekeeping: reap abandoned receiver partials, unlinking only
/// their junk `.part` bytes; a delivered `DONE` partial (the file the user
/// keeps) is spared. Sender retention is not aged here — its row is also what
/// lets the sender open their own attachment, so it lives as long as the
/// message does and expiry only closes *serving* (see [`serve`]).
pub fn gc(now: u64) {
    let _ = store::gc_dead_partials(now.saturating_sub(DEAD_PARTIAL_TTL_SECS));
}

/// Once at startup: drop retention nothing names. The live paths release a
/// file the moment its last holder goes, but a composer chip is held in
/// memory, so one that was readied and then died with the process left its
/// row and its copy behind with nobody to let go of them.
pub fn sweep_orphaned_retention() {
    let fids = store::retention_file_ids();
    crate::data::media::unlink_orphaned(&crate::db::messages::MESSAGES_DB.lock(), &fids);
}

/// Whether to pull an offered attachment without a user tap: only from a paired
/// contact, over a trusted (un-metered) network, and at or below [`AUTO_MAX`].
/// Pure policy so the receive arm and its test share one rule.
pub fn should_auto_download(ipk: &[u8; 32], size: u64, on_wifi: bool) -> bool {
    on_wifi && size <= AUTO_MAX && crate::data::contact::Contact::is_paired(ipk)
}

/// Builds the manifest for `path`, retains it (and the source location) so we
/// keep serving pulls until `ttl_secs` elapses, and returns the offer's
/// `(file_id, size)`.
pub fn prepare_send(path: &str, ttl_secs: u64) -> anyhow::Result<([u8; 32], u64)> {
    let m = wire::Manifest::from_file(path)?;
    let file_id = m.file_id();
    let size = m.total_size;
    let expires = crate::utils::systime().as_secs() + ttl_secs;
    store::retention_put(&file_id, path, size, m.chunk_size, &postcard::to_allocvec(&m)?, expires)?;
    Ok((file_id, size))
}

/// Answer requests in the grammar selected by this connection's TLS ALPN.
/// Legacy peers receive a raw contiguous suffix; v2 peers request bounded
/// missing ranges with indexed, verified chunks and explicit completion.
///
/// Every stream starts with the mutual [`auth`] handshake pinning the peer's
/// IPK to this connection's TLS key; a stream that fails it is dropped before
/// any pull is read.
pub async fn serve_link(link: crate::p2p::PeerLink) {
    let local = match auth::local_auth() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("transfer: cannot serve without local auth: {e}");
            return;
        },
    };
    serve_streams(link, local).await
}

/// Whether we offered `file_id` to `peer`. Retention is keyed by content hash
/// alone, so the outgoing message row is what scopes a pull — and its
/// `Manifest`/`Gone` answer — to who the file was actually sent to. In a group
/// that is every active member of the conversation it was posted in, not one
/// contact.
fn offered_to(file_id: &[u8; 32], peer: &[u8; 32], me: &[u8; 32]) -> bool {
    let paired = crate::data::contact::Contact::is_paired(peer);
    let db = crate::db::messages::MESSAGES_DB.lock();
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

/// [`serve_link`] minus the process-global identity, so a test can drive two
/// in-process endpoints with distinct constructed identities.
async fn serve_streams(link: crate::p2p::PeerLink, local: wire::Auth) {
    let mut streams = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = streams.join_next(), if !streams.is_empty() => {},
            accepted = link.accept_stream(), if streams.len() < SERVES_PER_LINK => {
                let Ok((mut s, mut r)) = accepted else { break };
                // Don't queue unbounded authenticated/unauthenticated work. A
                // busy stream resets; its caller can retry within its budget.
                let Ok(slot) = SERVING.try_acquire() else {
                    let _ = s.reset(0u32.into());
                    let _ = r.stop(0u32.into());
                    continue;
                };
                let link = link.clone();
                let local = local.clone();
                streams.spawn(async move {
                    let _slot = slot;
                    if let Err(e) = serve_stream(&link, &local, s, r).await {
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
    // JoinSet drop cancels remaining work when the connection ends.
}

async fn serve_stream(
    link: &crate::p2p::PeerLink, local: &wire::Auth, s: quinn::SendStream, mut r: quinn::RecvStream,
) -> Result<(), Failure> {
    let mut s = TransferSend::new(s);
    bounded(CONTROL_TIMEOUT, auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local)).await?;
    if link.protocol().map_err(|e| Failure::new(FailureKind::InvalidData, e))?
        == crate::p2p::protocol::AttachmentProtocol::V2
    {
        return serve_v2(link, local, &mut s, &mut r).await;
    }
    let pull: wire::Pull =
        bounded(CONTROL_TIMEOUT, wire::read_frame_limited(&mut r, wire::PULL_FRAME_LIMIT)).await?;
    let now = crate::utils::systime().as_secs();
    let retained = store::retention_get(&pull.file_id)
        .filter(|r| r.expires_at > now && offered_to(&pull.file_id, &link.ipk, &local.ipk));
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
        if !offered_to(&pull.file_id, &link.ipk, &local.ipk)
            || store::retention_get(&pull.file_id)
                .is_none_or(|r| r.expires_at <= crate::utils::systime().as_secs())
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

fn protocol_failure(message: &'static str) -> Failure {
    Failure::new(FailureKind::InvalidData, wire::InvalidFrame(message.into()))
}

fn remote_failure(code: v2::ErrorCode) -> Failure {
    let kind = match code {
        v2::ErrorCode::Unavailable => FailureKind::Unavailable,
        v2::ErrorCode::Busy => FailureKind::Transport,
        v2::ErrorCode::Storage => FailureKind::Storage,
        v2::ErrorCode::InvalidRequest | v2::ErrorCode::Unsupported => FailureKind::InvalidData,
    };
    Failure::new(kind, code)
}

/// A Complete response is terminal: trailing bytes are a protocol error, and
/// a kept-alive stream without FIN is bounded just like other control work.
async fn expect_fin(r: &mut quinn::RecvStream) -> Result<(), Failure> {
    bounded(CONTROL_TIMEOUT, async {
        let mut byte = [0; 1];
        if r.read(&mut byte).await?.is_some() {
            return Err(wire::InvalidFrame("data after completed request".into()).into());
        }
        Ok(())
    })
    .await
}

async fn v2_error(s: &mut TransferSend, code: v2::ErrorCode) -> Result<(), Failure> {
    bounded(CONTROL_TIMEOUT, v2::write_frame(s, &v2::Frame::Error(code))).await?;
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    Ok(())
}

/// V2 runs only after TLS-selected grammar and mutual identity verification.
/// Each stream owns one authorized Describe/Pull and its cancellation scope.
async fn serve_v2(
    link: &crate::p2p::PeerLink, local: &wire::Auth, s: &mut TransferSend,
    r: &mut quinn::RecvStream,
) -> Result<(), Failure> {
    let limits = bounded(CONTROL_TIMEOUT, v2::exchange_hello(s, r)).await?;
    let request = bounded(CONTROL_TIMEOUT, v2::read_frame_for(r, v2::ReadPhase::Request)).await?;
    expect_fin(r).await?;
    let (file_id, requested) = match request {
        v2::Frame::Describe { file_id } => (file_id, None),
        v2::Frame::Pull { file_id, ranges } => (file_id, Some(ranges)),
        _ => return v2_error(s, v2::ErrorCode::InvalidRequest).await,
    };
    if !offered_to(&file_id, &link.ipk, &local.ipk) {
        return v2_error(s, v2::ErrorCode::Unavailable).await;
    }
    let Some(retained) = store::retention_get(&file_id)
        .filter(|ret| ret.expires_at > crate::utils::systime().as_secs())
    else {
        return v2_error(s, v2::ErrorCode::Unavailable).await;
    };
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
            if !offered_to(&file_id, &link.ipk, &local.ipk)
                || store::retention_get(&file_id)
                    .is_none_or(|ret| ret.expires_at <= crate::utils::systime().as_secs())
            {
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

/// Pulls in flight by `file_id`. Two concurrent `download`s (auto-download
/// racing a manual tap) must not co-write one partial file; the loser no-ops
/// and the UI follows the winner through the `partials` doorbell.
static DOWNLOADING: Lazy<Mutex<HashSet<[u8; 32]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Releases the [`DOWNLOADING`] slot on every exit path — a leaked entry
/// would wedge the file_id forever.
struct PullGuard([u8; 32]);
impl Drop for PullGuard {
    fn drop(&mut self) {
        DOWNLOADING.lock().remove(&self.0);
    }
}

/// Pull `file_id` from the member who offered it: resolve the sender from
/// the media row, dial (or reuse) the P2P link, and run the resumable pull.
/// No-op when the file is already downloaded or a pull is in flight.
pub async fn download(file_id: [u8; 32]) -> anyhow::Result<()> {
    download_with_policy(file_id, DownloadTrigger::Requested).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DownloadTrigger {
    Requested,
    Reconnect,
    NetworkChange,
    WakeResponse,
}

async fn download_with_policy(file_id: [u8; 32], trigger: DownloadTrigger) -> anyhow::Result<()> {
    if !DOWNLOADING.lock().insert(file_id) {
        return Ok(());
    }
    let _guard = PullGuard(file_id);
    // Scans/spawns can race another worker finishing. Recheck terminal state
    // after owning the writer slot; only a new requested download may retry a
    // validation/authentication/storage failure.
    let current = store::partial_get(&file_id);
    if current.as_ref().is_some_and(|p| p.is_complete()) {
        return Ok(());
    }
    if matches!(trigger, DownloadTrigger::Reconnect | DownloadTrigger::NetworkChange) {
        if current.as_ref().is_none_or(|p| p.state == store::FAILED) {
            return Ok(());
        }
        if trigger == DownloadTrigger::Reconnect
            && store::retry_after(&file_id) > crate::utils::systime().as_secs()
        {
            return Ok(());
        }
    }
    if trigger == DownloadTrigger::WakeResponse && !store::claim_ready_retry(&file_id)? {
        return Ok(());
    }
    // Register before looking up the message: deletion before registration is
    // caught by the lookup, deletion after it cancels this generation.
    let lease = store::receiver_lease(file_id);
    let _slot = tokio::select! {
        slot = PULLING.acquire() => slot?,
        _ = lease.cancel.cancelled() => return Ok(()),
    };
    let Some((peer, offered_size)) = crate::data::media::attachment_offer(&file_id)? else {
        if lease.cancel.is_cancelled() {
            return Ok(());
        }
        // No incoming provider does not imply no holder: an outgoing message
        // or staged composer may still own this content-addressed file.
        if let Some(p) = store::partial_get(&file_id) {
            set_state(&file_id, p.source_ipk, store::FAILED, &lease)?;
        }
        crate::data::media::unlink_orphaned(&crate::db::messages::MESSAGES_DB.lock(), &[file_id]);
        anyhow::bail!("no incoming offer for that file_id");
    };
    let local = match auth::local_auth() {
        Ok(local) => local,
        Err(e) => {
            set_state(&file_id, peer, store::FAILED, &lease)?;
            report_failure(FailureKind::Authentication);
            return Err(e);
        },
    };
    let held =
        drive_download(file_id, peer, offered_size, &local, &lease, &RETRY_DELAYS, || async {
            crate::p2p::link(peer).await.map_err(Failure::wire)
        })
        .await?;
    // Release the writer before sending FileWant. A fast dial-back can now
    // start its pull immediately instead of losing on_link_ready to our guard.
    drop(_slot);
    drop(lease);
    drop(_guard);
    if held
        && trigger != DownloadTrigger::WakeResponse
        && store::claim_wake(&file_id, crate::utils::systime().as_secs(), WAKE_BACKOFF_SECS)?
    {
        let paired = crate::data::contact::Contact::is_paired(&peer);
        if let Some(conversation) =
            crate::data::conversation::Conversation::for_peer_transport(&peer, paired)
        {
            let _ = timeout(
                CONTROL_TIMEOUT,
                crate::messaging::send_control_wake_to(
                    conversation,
                    common::proto::mls_wire::AppPayload::FileWant { file_id },
                    peer,
                ),
            )
            .await;
        }
    }
    Ok(())
}

async fn drive_download<F, Fut>(
    file_id: [u8; 32], peer: [u8; 32], offered_size: u64, local: &wire::Auth,
    lease: &store::ReceiverLease, retry_delays: &[Duration], mut connect: F,
) -> anyhow::Result<bool>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<crate::p2p::PeerLink, Failure>>,
{
    for attempt in 0..=retry_delays.len() {
        if lease.cancel.is_cancelled() {
            return Ok(false);
        }
        if matches!(crate::p2p::consent::may_connect(&peer), crate::p2p::consent::Decision::No) {
            set_state(&file_id, peer, store::FAILED, &lease)?;
            report_failure(FailureKind::Unavailable);
            anyhow::bail!("attachment sender is no longer permitted");
        }
        set_state(&file_id, peer, store::CONNECTING, &lease)?;
        let attempt_result = tokio::select! {
            _ = lease.cancel.cancelled() => return Ok(false),
            result = async {
                let link = connect().await?;
                let result = pull_live(&link, file_id, offered_size, &local, &lease, CHUNK_TIMEOUT).await;
                if result.as_ref().is_err_and(|e|
                    matches!(e.kind, FailureKind::Transport | FailureKind::Authentication)
                        && e.source.downcast_ref::<v2::ErrorCode>() != Some(&v2::ErrorCode::Busy)
                ) {
                    // Close only this connection; another worker may already
                    // have established a newer one for the same peer. Busy is
                    // an explicit response, not a broken shared connection.
                    link.conn.close(0u32.into(), b"transfer link invalidated");
                }
                result
            } => result,
        };
        match attempt_result {
            Ok(()) => {
                diagnostics::record(Event::TransferComplete);
                return Ok(false);
            },
            Err(e) if e.kind == FailureKind::Cancelled => return Ok(false),
            Err(e) if e.kind != FailureKind::Transport => {
                report_failure(e.kind);
                set_state(&file_id, peer, store::FAILED, &lease)?;
                return Err(e.into());
            },
            Err(e) => {
                log::debug!("transfer: transient failure, attempt {}: {e}", attempt + 1);
                set_state(&file_id, peer, store::HELD, &lease)?;
                if let Some(delay) = retry_delays.get(attempt) {
                    diagnostics::record(Event::TransportRetry);
                    tokio::select! {
                        _ = lease.cancel.cancelled() => return Ok(false),
                        _ = tokio::time::sleep(*delay) => {},
                    }
                } else {
                    diagnostics::record(Event::RetryExhausted);
                    store::defer_retry(
                        &file_id,
                        crate::utils::systime().as_secs() + RETRY_COOLDOWN_SECS,
                    )?;
                }
            },
        }
    }
    Ok(true)
}

/// Update progress without replacing independently persisted wake/retry history.
fn set_state(
    file_id: &[u8; 32], peer: [u8; 32], state: u8, lease: &store::ReceiverLease,
) -> anyhow::Result<()> {
    let mut p = store::partial_get(file_id).unwrap_or(store::Partial {
        file_id: *file_id,
        source_ipk: peer,
        total: 0,
        chunk_size: 0,
        manifest: None,
        have: 0,
        state,
        path: store::partial_path(file_id),
        updated_at: 0,
    });
    p.state = state;
    p.source_ipk = peer;
    p.updated_at = crate::utils::systime().as_secs();
    store::partial_put_live(&p, lease)
}

/// Re-drive every incomplete pull from `peer`: they just became reachable,
/// whether the relay said so or a link to them formed. Held pulls otherwise
/// waited for our own next reconnect, which on a phone that stays online is
/// never.
pub fn resume_for_peer(peer: [u8; 32]) {
    for file_id in store::incomplete_file_ids_for(&peer) {
        crate::RUNTIME.spawn(async move {
            if let Err(e) = download_with_policy(file_id, DownloadTrigger::Reconnect).await {
                log::warn!("transfer: resume {} failed: {e}", hex::encode(&file_id[..4]));
            }
        });
    }
}

/// A direct link to `peer` just opened, in either direction.
pub fn on_link_ready(peer: [u8; 32]) {
    // Permit one early resume after a claimed wake. Otherwise retain the
    // cooldown: concurrent failing files must not restart one another each
    // time either one reconnects the shared peer link.
    for file_id in store::incomplete_file_ids_for(&peer) {
        crate::RUNTIME.spawn(async move {
            if let Err(e) = download_with_policy(file_id, DownloadTrigger::WakeResponse).await {
                log::debug!("transfer: ready-link resume failed: {e}");
            }
        });
    }
}

/// Handle an inbound reverse-wake: a contact wants a file we offered but
/// couldn't reach us for. The platform push already revived the app. They
/// gave up dialing when they sent this, so the link is ours to open: once it
/// forms, their side pulls whatever it was holding for us.
pub fn on_file_want(peer: [u8; 32], file_id: [u8; 32]) {
    let Some(me) = crate::data::identity::Identity::get().map(|i| i.ipk()) else { return };
    if !offered_to(&file_id, &peer, &me) {
        return;
    }
    log::info!(
        "transfer: FileWant from {} for {}",
        hex::encode(&peer[..4]),
        hex::encode(&file_id[..4]),
    );
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::p2p::link(peer).await {
            log::debug!("transfer: dial back to {} failed: {e}", hex::encode(&peer[..4]));
        }
    });
}

/// Re-drive every incomplete pull on reconnect (and app-open, which reconnects):
/// HELD (sender was offline last time) and ACTIVE (a transfer whose process died
/// mid-pull, so nothing is driving it now — the usual "stuck at 65% after a
/// restart"). One spawn per file_id — the `DOWNLOADING` guard dedups a racing
/// user tap or a genuinely-live pull, and [`download`] resumes from the stored
/// `have` watermark, completing if the sender is up or falling to HELD if not.
/// FAILED is excluded: a real error must not auto-loop.
pub async fn resume_incomplete_downloads() {
    for file_id in store::incomplete_file_ids() {
        crate::RUNTIME.spawn(async move {
            if let Err(e) = download_with_policy(file_id, DownloadTrigger::Reconnect).await {
                log::warn!("transfer: resume {} failed: {e}", hex::encode(&file_id[..4]));
            }
        });
    }
}

/// A confirmed platform network change makes the previous path's cooldown
/// obsolete. It may start a new bounded episode for incomplete work, while
/// terminal failures and live writers remain excluded. LinkReady cannot call
/// this path and therefore cannot recursively replenish retry budgets.
pub(crate) async fn on_network_changed() {
    for file_id in store::incomplete_file_ids() {
        crate::RUNTIME.spawn(async move {
            if let Err(e) = download_with_policy(file_id, DownloadTrigger::NetworkChange).await {
                log::debug!("transfer: network-change resume failed: {e}");
            }
        });
    }
}

/// The wire+disk half of [`download`] over an already-open link, split out so
/// a test can drive it against [`serve_link`] on a direct loopback pair
/// (acquiring a real link needs the full punch choreography).
///
/// Crash-safety contract: a chunk's bytes are synced to disk BEFORE the
/// verified bitmap and contiguous prefix are persisted. Recovery rehashes
/// those candidates before requesting the remaining bytes.
#[cfg(test)]
async fn pull(
    link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64, local: &wire::Auth,
) -> Result<(), Failure> {
    let lease = store::receiver_lease(file_id);
    pull_live(link, file_id, offered_size, local, &lease, CHUNK_TIMEOUT).await
}

async fn pull_live(
    link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64, local: &wire::Auth,
    lease: &store::ReceiverLease, chunk_timeout: Duration,
) -> Result<(), Failure> {
    tokio::select! {
        biased;
        _ = lease.cancel.cancelled() => Err(Failure::new(FailureKind::Cancelled, store::Cancelled)),
        result = async {
            match link.protocol().map_err(|e| Failure::new(FailureKind::InvalidData, e))? {
                crate::p2p::protocol::AttachmentProtocol::Legacy => {
                    pull_legacy(link, file_id, offered_size, local, lease, chunk_timeout).await
                },
                crate::p2p::protocol::AttachmentProtocol::V2 => {
                    pull_v2(link, file_id, offered_size, local, lease).await
                },
            }
        } => result,
    }
}

/// The old wire still means one contiguous prefix. Sparse local progress must
/// never be advertised as that prefix; the shared coordinator revalidates it.
async fn pull_legacy(
    link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64, local: &wire::Auth,
    lease: &store::ReceiverLease, chunk_timeout: Duration,
) -> Result<(), Failure> {
    let cached_manifest = store::partial_get(&file_id)
        .and_then(|p| p.manifest)
        .filter(|bytes| bytes.len() <= 8 * 1024 * 1024)
        .and_then(|bytes| postcard::from_bytes::<wire::Manifest>(&bytes).ok())
        .filter(|m| m.file_id() == file_id && m.total_size == offered_size);
    let mut cached = match cached_manifest {
        Some(ref manifest) => Some(
            ranges::Receiver::open_async(file_id, link.ipk, manifest.clone(), offered_size, lease)
                .await
                .map_err(coordinator_failure)?,
        ),
        None => None,
    };
    let have0 = cached.as_ref().map_or(0, |receiver| receiver.prefix());
    let (s, mut r) = bounded(CONTROL_TIMEOUT, link.open_stream()).await?;
    let mut s = TransferSend::new(s);
    bounded(CONTROL_TIMEOUT, auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local)).await?;
    bounded(CONTROL_TIMEOUT, wire::write_frame(&mut s, &wire::Pull { file_id, have: have0 }))
        .await?;
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    let manifest =
        match bounded(CONTROL_TIMEOUT, wire::read_frame::<wire::ServeResp>(&mut r)).await? {
            wire::ServeResp::Manifest(m) => m,
            wire::ServeResp::Gone => {
                return Err(Failure::new(
                    FailureKind::Unavailable,
                    anyhow::anyhow!("sender no longer retains the file"),
                ));
            },
        };
    if manifest.file_id() != file_id
        || manifest.chunk_size == 0
        || manifest.chunk_size as usize > wire::CHUNK_SIZE
        || manifest.chunks.len() as u64 != manifest.total_size.div_ceil(manifest.chunk_size as u64)
        || have0 as usize > manifest.chunks.len()
        || manifest.total_size != offered_size
    {
        return Err(protocol_failure("manifest does not match the attachment offer"));
    }
    let mut receiver = match cached.take() {
        Some(receiver) => receiver,
        None => {
            ranges::Receiver::open_async(file_id, link.ipk, manifest.clone(), offered_size, lease)
                .await
                .map_err(coordinator_failure)?
        },
    };
    let mut buf = vec![0; manifest.chunk_size as usize];
    for index in have0 as usize..manifest.chunks.len() {
        let expected = (manifest.total_size - index as u64 * manifest.chunk_size as u64)
            .min(manifest.chunk_size as u64) as usize;
        tokio::select! {
            biased;
            _ = lease.cancel.cancelled() => return Err(Failure::new(FailureKind::Cancelled, store::Cancelled)),
            result = read_chunk(&mut r, &mut buf[..expected], chunk_timeout) => result?,
        }
        let already_verified = receiver.contains(index as u32);
        receiver.commit(index as u32, &buf[..expected], lease).map_err(coordinator_failure)?;
        if !already_verified {
            diagnostics::received_verified(expected as u64);
        }
    }
    receiver.finish(lease).map_err(coordinator_failure)?;
    store::defer_retry(&file_id, 0).map_err(Failure::storage)?;
    Ok(())
}

fn coordinator_failure(error: anyhow::Error) -> Failure {
    if error.is::<wire::InvalidFrame>() { Failure::wire(error) } else { Failure::storage(error) }
}

async fn open_v2_request(
    link: &crate::p2p::PeerLink, local: &wire::Auth,
) -> Result<(TransferSend, quinn::RecvStream, v2::Limits), Failure> {
    let (s, mut r) = bounded(CONTROL_TIMEOUT, link.open_stream()).await?;
    let mut s = TransferSend::new(s);
    bounded(CONTROL_TIMEOUT, auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local)).await?;
    let limits = bounded(CONTROL_TIMEOUT, v2::exchange_hello(&mut s, &mut r)).await?;
    Ok((s, r, limits))
}

async fn pull_v2(
    link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64, local: &wire::Auth,
    lease: &store::ReceiverLease,
) -> Result<(), Failure> {
    let (mut s, mut r, _) = open_v2_request(link, local).await?;
    bounded(CONTROL_TIMEOUT, v2::write_frame(&mut s, &v2::Frame::Describe { file_id })).await?;
    s.finish().map_err(|e| Failure::wire(e.into()))?;
    let manifest = match bounded(
        CONTROL_TIMEOUT,
        v2::read_frame_for(&mut r, v2::ReadPhase::Manifest),
    )
    .await?
    {
        v2::Frame::Manifest(manifest) => manifest,
        v2::Frame::Error(code) => return Err(remote_failure(code)),
        _ => return Err(protocol_failure("expected attachment manifest")),
    };
    if manifest.file_id() != file_id || manifest.total_size != offered_size {
        return Err(protocol_failure("manifest does not match the attachment offer"));
    }
    match bounded(CONTROL_TIMEOUT, v2::read_frame_for(&mut r, v2::ReadPhase::Complete)).await? {
        v2::Frame::Complete { chunks_sent: 0 } => {},
        v2::Frame::Error(code) => return Err(remote_failure(code)),
        _ => return Err(protocol_failure("invalid manifest completion")),
    }
    expect_fin(&mut r).await?;
    let mut receiver =
        ranges::Receiver::open_async(file_id, link.ipk, manifest.clone(), offered_size, lease)
            .await
            .map_err(coordinator_failure)?;
    while receiver.prefix() < manifest.chunks.len() as u32 {
        let (mut s, mut r, limits) = open_v2_request(link, local).await?;
        let requested = receiver.missing(limits.max_ranges as usize, limits.max_chunks as u32);
        let expected = v2::validate_ranges(&requested, &manifest, limits).map_err(Failure::wire)?;
        let mut pending: Vec<u32> =
            requested.iter().flat_map(|range| range.start..range.end).collect();
        pending.reverse();
        bounded(
            CONTROL_TIMEOUT,
            v2::write_frame(&mut s, &v2::Frame::Pull { file_id, ranges: requested }),
        )
        .await?;
        s.finish().map_err(|e| Failure::wire(e.into()))?;
        while let Some(expected_index) = pending.pop() {
            match bounded(CHUNK_DEADLINE, v2::read_frame_for(&mut r, v2::ReadPhase::Chunk)).await? {
                v2::Frame::Chunk { index, bytes } if index == expected_index => {
                    receiver.commit(index, &bytes, lease).map_err(coordinator_failure)?;
                    diagnostics::received_verified(bytes.len() as u64);
                },
                v2::Frame::Error(code) => return Err(remote_failure(code)),
                _ => {
                    return Err(protocol_failure(
                        "unexpected, duplicate or missing attachment chunk",
                    ));
                },
            }
        }
        match bounded(CONTROL_TIMEOUT, v2::read_frame_for(&mut r, v2::ReadPhase::Complete)).await? {
            v2::Frame::Complete { chunks_sent } if chunks_sent == expected => {},
            v2::Frame::Error(code) => return Err(remote_failure(code)),
            _ => return Err(protocol_failure("invalid range completion")),
        }
        expect_fin(&mut r).await?;
    }
    receiver.finish(lease).map_err(coordinator_failure)?;
    store::defer_retry(&file_id, 0).map_err(Failure::storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_send_retains_manifest() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let path = std::env::temp_dir().join("promtuz-prepare_send.bin");
        std::fs::write(&path, vec![0x11u8; 300 * 1024]).unwrap();

        let (file_id, size) = prepare_send(path.to_str().unwrap(), 3600).unwrap();
        assert_eq!(size, 300 * 1024);
        assert!(store::retention_get(&file_id).is_some());
    }

    #[test]
    fn auto_download_only_paired_wifi_and_small() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        use crate::data::contact::Contact;
        let paired = [0xa1u8; 32];
        Contact::save_pending(paired, "peer".into()).unwrap();
        Contact::mark_paired(&paired);
        assert!(Contact::is_paired(&paired));
        let unpaired = [0xa2u8; 32];

        assert!(should_auto_download(&paired, AUTO_MAX, true), "paired + wifi + at-cap");
        assert!(!should_auto_download(&unpaired, AUTO_MAX, true), "unpaired never");
        assert!(!should_auto_download(&paired, AUTO_MAX, false), "metered never");
        assert!(!should_auto_download(&paired, AUTO_MAX + 1, true), "oversize never");
    }

    /// A partial whose chat is gone has nothing to resume toward — and nothing
    /// downstream to mark it FAILED for the gc — so the re-drive is what reaps it.
    #[tokio::test]
    async fn download_with_no_media_row_forgets_the_partial() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let fid = [0xf3u8; 32];
        store::partial_put(&store::Partial {
            file_id: fid,
            source_ipk: [1u8; 32],
            total: 100,
            chunk_size: 50,
            manifest: None,
            have: 1,
            state: store::ACTIVE,
            path: store::partial_path(&fid),
            updated_at: 0,
        })
        .unwrap();

        assert!(download(fid).await.is_err(), "no message names it");
        assert!(store::partial_get(&fid).is_none(), "ghost row reaped");
    }
    #[tokio::test]
    async fn confirmed_network_change_bypasses_cooldown_but_not_terminal_or_owned_work() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let fid = [0xda; 32];
        store::forget_partial(&fid);
        let lease = store::receiver_lease(fid);
        set_state(&fid, [0xdb; 32], store::HELD, &lease).unwrap();
        store::defer_retry(&fid, crate::utils::systime().as_secs() + 60).unwrap();
        download_with_policy(fid, DownloadTrigger::Reconnect).await.unwrap();
        assert_eq!(store::partial_get(&fid).unwrap().state, store::HELD);
        assert!(DOWNLOADING.lock().insert(fid));
        let guard = PullGuard(fid);
        download_with_policy(fid, DownloadTrigger::NetworkChange).await.unwrap();
        assert_eq!(store::partial_get(&fid).unwrap().state, store::HELD, "live writer retained");
        drop(guard);
        set_state(&fid, [0xdb; 32], store::FAILED, &lease).unwrap();
        download_with_policy(fid, DownloadTrigger::NetworkChange).await.unwrap();
        assert_eq!(
            store::partial_get(&fid).unwrap().state,
            store::FAILED,
            "terminal failure retained"
        );
        set_state(&fid, [0xdb; 32], store::HELD, &lease).unwrap();
        drop(lease);
        // With no incoming offer in this fixture, reaching the lookup proves
        // this confirmed external event began its episode despite cooldown.
        let error = download_with_policy(fid, DownloadTrigger::NetworkChange).await.unwrap_err();
        assert!(error.to_string().contains("no incoming offer"));
        assert!(store::partial_get(&fid).is_none(), "orphan cleanup actually ran");
    }

    #[tokio::test]
    async fn racing_ready_callback_does_not_spend_an_active_writers_wake_allowance() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let fid = [0xcd; 32];
        store::forget_partial(&fid);
        let lease = store::receiver_lease(fid);
        set_state(&fid, [0xce; 32], store::HELD, &lease).unwrap();
        assert!(store::claim_wake(&fid, 100, 60).unwrap());
        assert!(DOWNLOADING.lock().insert(fid));
        let guard = PullGuard(fid);
        download_with_policy(fid, DownloadTrigger::WakeResponse).await.unwrap();
        assert!(
            store::claim_ready_retry(&fid).unwrap(),
            "the losing callback must not consume the allowance"
        );
        drop(guard);
        store::forget_partial(&fid);
    }
}

#[cfg(test)]
mod download_resume {
    use std::net::Ipv6Addr;

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::p2p::PeerLink;

    /// Both loopback endpoints present this TLS key ([`linked_pair`] builds
    /// them from it), so a test Auth's `tls_pub` must vouch for it.
    const TLS_SEED: [u8; 32] = [7u8; 32];

    /// A per-endpoint identity for the handshake: the IPK from `ipk_seed`
    /// signing the binding over the shared loopback TLS key.
    fn identity(ipk_seed: [u8; 32]) -> wire::Auth {
        use ed25519_dalek::Signer;
        let ipk_key = SigningKey::from_bytes(&ipk_seed);
        let tls_pub = SigningKey::from_bytes(&TLS_SEED).verifying_key().to_bytes();
        let msg = crate::quic::peer_config::ipk_binding_message(&tls_pub);
        wire::Auth {
            ipk: ipk_key.verifying_key().to_bytes(),
            tls_pub,
            sig: ipk_key.sign(&msg).to_bytes(),
        }
    }

    fn paired_identity(ipk_seed: [u8; 32]) -> wire::Auth {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let a = identity(ipk_seed);
        crate::data::contact::Contact::save_pending(a.ipk, "peer".into()).unwrap();
        crate::data::contact::Contact::mark_paired(&a.ipk);
        a
    }

    /// Record the outgoing attachment message that scopes `file_id`'s
    /// retention to `peer` — what `send_attachment` persists before the offer
    /// goes out, and what `serve_streams` answers a pull against.
    fn offer_to(peer: [u8; 32], file_id: [u8; 32]) {
        let conv = crate::data::conversation::Conversation::for_peer(&peer).unwrap();
        // Deliberately omit our roster entry, as pre-conversation migrations did.
        offer_in(conv, file_id);
    }

    pub(super) fn offer_in(conv: [u8; 16], file_id: [u8; 32]) {
        let row = crate::data::media::MediaRow {
            kind: crate::data::media::KIND_ATTACHMENT,
            group_id: None,
            mime: "application/octet-stream".into(),
            name: "f.bin".into(),
            size: 0,
            width: 0,
            height: 0,
            blob: None,
            thumb: None,
            file_id: Some(file_id.to_vec()),
            duration_ms: 0,
            sticker: None,
        };
        crate::data::media::save_outgoing_with_media(&conv, "", None, &row).unwrap();
    }

    /// Two directly-connected peer endpoints on loopback — the real QUIC
    /// stack minus the punch layer, which a unit test can't drive. Each
    /// link's `ipk` is the peer that side expects on its streams.
    async fn linked_pair(
        a_expects: [u8; 32], b_expects: [u8; 32],
    ) -> (PeerLink, PeerLink, quinn::Endpoint, quinn::Endpoint) {
        let _ = common::quic::config::setup_crypto_provider();
        let key = SigningKey::from_bytes(&TLS_SEED);
        let (server_cfg, client_cfg) = crate::quic::peer_config::test_peer_configs(&key).unwrap();
        let ep_a = quinn::Endpoint::server(server_cfg, (Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        let mut ep_b = quinn::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        ep_b.set_default_client_config(client_cfg);
        let dial = ep_b.connect(ep_a.local_addr().unwrap(), "peer").unwrap();
        let (conn_a, conn_b) = tokio::join!(
            async { ep_a.accept().await.unwrap().accept().unwrap().await.unwrap() },
            async { dial.await.unwrap() },
        );
        (
            crate::p2p::test_link(conn_a, a_expects),
            crate::p2p::test_link(conn_b, b_expects),
            ep_a,
            ep_b,
        )
    }

    fn recovery_identity(id: u8) -> wire::Auth {
        let mut seed = [0xd7; 32];
        seed[0] = id;
        paired_identity(seed)
    }

    async fn wait_for_prefix(fid: [u8; 32], have: u32) {
        timeout(Duration::from_secs(3), async {
            loop {
                if store::partial_get(&fid).is_some_and(|p| p.have >= have) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn interrupted_pull_reconnects_and_resumes_verified_prefix_automatically() {
        let id_a = recovery_identity(81);
        let id_b = recovery_identity(82);
        let (server1, client1, _ep1, _ep2) = linked_pair(id_b.ipk, id_a.ipk).await;
        let (server2, client2, _ep3, _ep4) = linked_pair(id_b.ipk, id_a.ipk).await;
        let src = std::env::temp_dir().join("promtuz-recovery-resume.bin");
        let mut bytes = vec![0x81; wire::CHUNK_SIZE * 2 + 128];
        bytes[wire::CHUNK_SIZE..].fill(0x82);
        std::fs::write(&src, &bytes).unwrap();
        let (fid, size) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(id_b.ipk, fid);
        store::forget_partial(&fid);
        let manifest = wire::Manifest::from_file(src.to_str().unwrap()).unwrap();
        let first_bytes = bytes[..wire::CHUNK_SIZE + 19].to_vec();
        let auth_a = id_a.clone();
        let first = tokio::spawn(async move {
            let (mut s, mut r) = server1.accept_stream().await.unwrap();
            auth::exchange(&server1.conn, &mut s, &mut r, server1.ipk, &auth_a).await.unwrap();
            let request: wire::Pull = wire::read_frame(&mut r).await.unwrap();
            assert_eq!(request.have, 0);
            wire::write_frame(&mut s, &wire::ServeResp::Manifest(manifest)).await.unwrap();
            s.write_all(&first_bytes).await.unwrap();
            wait_for_prefix(fid, 1).await;
            server1.conn.close(0u32.into(), b"test network interruption");
        });
        let second = tokio::spawn(serve_streams(server2, id_a.clone()));
        let mut links = std::collections::VecDeque::from([client1, client2]);
        let lease = store::receiver_lease(fid);
        let mut attempts = 0;
        drive_download(
            fid,
            id_a.ipk,
            size,
            &id_b,
            &lease,
            &[Duration::ZERO, Duration::ZERO],
            || {
                attempts += 1;
                if attempts == 2 {
                    assert_eq!(
                        store::partial_get(&fid).unwrap().have,
                        1,
                        "verified prefix survives broken connection"
                    );
                }
                let next = links.pop_front().unwrap();
                async move { Ok(next) }
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts, 2);
        first.await.unwrap();
        let completed = store::partial_get(&fid).unwrap();
        assert_eq!(completed.state, store::DONE);
        assert_eq!(std::fs::read(&completed.path).unwrap(), bytes);
        second.abort();
    }

    #[tokio::test]
    async fn missing_incoming_offer_preserves_an_outgoing_copy() {
        let peer = recovery_identity(91);
        let src = std::env::temp_dir().join("promtuz-recovery-shared.bin");
        std::fs::write(&src, [0x91; 100]).unwrap();
        let (fid, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(peer.ipk, fid);
        assert!(download(fid).await.is_err(), "there is no incoming provider");
        assert!(store::retention_get(&fid).is_some());
        assert_eq!(std::fs::read(&src).unwrap(), vec![0x91; 100]);
    }

    #[tokio::test]
    async fn stalled_chunk_is_transient_and_deletion_cancels_waiting_pull() {
        let id_a = recovery_identity(83);
        let id_b = recovery_identity(84);
        let (server, client, _ep1, _ep2) = linked_pair(id_b.ipk, id_a.ipk).await;
        let src = std::env::temp_dir().join("promtuz-recovery-stall.bin");
        std::fs::write(&src, vec![0x83; 100]).unwrap();
        let manifest = wire::Manifest::from_file(src.to_str().unwrap()).unwrap();
        let fid = manifest.file_id();
        store::forget_partial(&fid);
        let serving = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (mut s, mut r) = server.accept_stream().await.unwrap();
                auth::exchange(&server.conn, &mut s, &mut r, server.ipk, &id_a).await.unwrap();
                let _: wire::Pull = wire::read_frame(&mut r).await.unwrap();
                wire::write_frame(&mut s, &wire::ServeResp::Manifest(manifest.clone()))
                    .await
                    .unwrap();
                // Keep the connection alive and the data stream unfinished.
                held.push((s, r));
            }
        });
        let lease = store::receiver_lease(fid);
        let err = pull_live(&client, fid, 100, &id_b, &lease, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::Transport);
        assert_eq!(store::partial_get(&fid).unwrap().have, 0);
        let delete = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let deleted_path = store::partial_get(&fid).unwrap().path;
            store::forget_partial(&fid);
            deleted_path
        };
        let pull = pull_live(&client, fid, 100, &id_b, &lease, Duration::from_secs(60));
        let (result, deleted_path) = tokio::join!(pull, delete);
        assert_eq!(result.unwrap_err().kind, FailureKind::Cancelled);
        assert!(store::partial_get(&fid).is_none());
        assert!(!std::path::Path::new(&deleted_path).exists());
        serving.abort();
    }

    #[tokio::test]
    async fn slow_chunk_progress_does_not_trigger_idle_timeout() {
        let id_a = recovery_identity(89);
        let id_b = recovery_identity(90);
        let (server, client, _ep1, _ep2) = linked_pair(id_b.ipk, id_a.ipk).await;
        let src = std::env::temp_dir().join("promtuz-recovery-slow.bin");
        let bytes = vec![0x89; 100];
        std::fs::write(&src, &bytes).unwrap();
        let manifest = wire::Manifest::from_file(src.to_str().unwrap()).unwrap();
        let fid = manifest.file_id();
        store::forget_partial(&fid);
        let serving = tokio::spawn(async move {
            let (mut s, mut r) = server.accept_stream().await.unwrap();
            auth::exchange(&server.conn, &mut s, &mut r, server.ipk, &id_a).await.unwrap();
            let _: wire::Pull = wire::read_frame(&mut r).await.unwrap();
            wire::write_frame(&mut s, &wire::ServeResp::Manifest(manifest)).await.unwrap();
            for chunk in bytes.chunks(10) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                s.write_all(chunk).await.unwrap();
            }
            s.finish().unwrap();
            s.stopped().await.unwrap();
        });
        let lease = store::receiver_lease(fid);
        // The entire chunk takes ~200ms; no idle gap is anywhere near100ms.
        pull_live(&client, fid, 100, &id_b, &lease, Duration::from_millis(100)).await.unwrap();
        let completed = store::partial_get(&fid).unwrap();
        assert_eq!(completed.state, store::DONE);
        assert_eq!(std::fs::read(&completed.path).unwrap(), vec![0x89; 100]);
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_auth_header_is_rejected_without_waiting_for_payload() {
        let id_a = recovery_identity(92);
        let id_b = recovery_identity(93);
        let (server, client, _ep1, _ep2) = linked_pair(id_b.ipk, id_a.ipk).await;
        let authenticating = tokio::spawn(async move {
            let (mut s, mut r) = server.accept_stream().await.unwrap();
            auth::exchange(&server.conn, &mut s, &mut r, server.ipk, &id_a).await
        });
        let (mut s, r) = client.open_stream().await.unwrap();
        s.write_all(&((wire::AUTH_FRAME_LIMIT + 1) as u32).to_le_bytes()).await.unwrap();
        // No payload or FIN: waiting for the claimed bytes would hang here.
        let error =
            timeout(Duration::from_secs(2), authenticating).await.unwrap().unwrap().unwrap_err();
        assert!(error.is::<wire::InvalidFrame>());
        drop((s, r));
    }

    #[tokio::test]
    async fn stalled_auth_does_not_block_another_stream_on_the_link() {
        let id_a = recovery_identity(85);
        let id_b = recovery_identity(86);
        let (server, client, _ep1, _ep2) = linked_pair(id_b.ipk, id_a.ipk).await;
        let serving = tokio::spawn(serve_streams(server, id_a));
        let (mut stalled_s, stalled_r) = client.open_stream().await.unwrap();
        // QUIC exposes the stream only after bytes are sent. Deliberately
        // provide only one of the four frame-length bytes.
        stalled_s.write_all(&[1]).await.unwrap();
        let src = std::env::temp_dir().join("promtuz-recovery-concurrent.bin");
        std::fs::write(&src, [0x84; 100]).unwrap();
        let (fid, size) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(id_b.ipk, fid);
        store::forget_partial(&fid);
        timeout(Duration::from_secs(2), pull(&client, fid, size, &id_b)).await.unwrap().unwrap();
        let completed = store::partial_get(&fid).unwrap();
        assert_eq!(std::fs::read(&completed.path).unwrap(), vec![0x84; 100]);
        drop((stalled_s, stalled_r));
        serving.abort();
    }

    #[tokio::test]
    async fn only_transient_failures_retry_and_retry_budget_is_finite() {
        let peer = recovery_identity(87).ipk;
        let local = identity([88; 32]);
        for (offset, kind) in [
            FailureKind::Transport,
            FailureKind::Authentication,
            FailureKind::InvalidData,
            FailureKind::Storage,
            FailureKind::Unavailable,
        ]
        .into_iter()
        .enumerate()
        {
            let fid = [0xb0 + offset as u8; 32];
            store::forget_partial(&fid);
            let lease = store::receiver_lease(fid);
            let mut attempts = 0;
            let result = drive_download(
                fid,
                peer,
                100,
                &local,
                &lease,
                &[Duration::ZERO, Duration::ZERO],
                || {
                    attempts += 1;
                    async move { Err(Failure::new(kind, anyhow::anyhow!("injected failure"))) }
                },
            )
            .await;
            assert_eq!(attempts, if kind == FailureKind::Transport { 3 } else { 1 });
            let state = store::partial_get(&fid).unwrap().state;
            if kind == FailureKind::Transport {
                assert!(result.is_ok());
                assert_eq!(state, store::HELD);
                assert!(store::retry_after(&fid) > crate::utils::systime().as_secs());
                // A reconnect during the persisted cooldown never reaches a
                // lookup/dial; this fixture deliberately has no incoming offer.
                download_with_policy(fid, DownloadTrigger::Reconnect).await.unwrap();
                assert!(store::partial_get(&fid).is_some());
            } else {
                assert!(result.is_err());
                assert_eq!(state, store::FAILED);
                download_with_policy(fid, DownloadTrigger::Reconnect).await.unwrap();
                download_with_policy(fid, DownloadTrigger::WakeResponse).await.unwrap();
                assert_eq!(
                    store::partial_get(&fid).unwrap().state,
                    store::FAILED,
                    "a stale automatic scan cannot retry a terminal failure"
                );
            }
            store::forget_partial(&fid);
        }
    }

    #[tokio::test]
    async fn pull_verifies_resumes_and_promotes() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id_a = paired_identity([51u8; 32]);
        let id_b = paired_identity([52u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        // Distinct per-chunk content so a mis-aligned resume can't verify.
        let src = std::env::temp_dir().join("promtuz-dl-src.bin");
        let mut bytes = vec![0xaau8; 300 * 1024];
        bytes[wire::CHUNK_SIZE..].fill(0xbb);
        std::fs::write(&src, &bytes).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(id_b.ipk, file_id);
        // The data dir outlives the run, and a DONE row left by the last one
        // would make this "fresh" pull skip every chunk.
        store::forget_partial(&file_id);

        // Fresh pull: every chunk lands, verifies, and the partial promotes.
        pull(&link_b, file_id, 300 * 1024, &id_b).await.unwrap();
        let p = store::partial_get(&file_id).unwrap();
        assert_eq!(p.state, store::DONE);
        assert_eq!(p.have, 2);
        assert_eq!(std::fs::read(&p.path).unwrap(), bytes);
        assert_eq!(wire::Manifest::from_file(&p.path).unwrap().file_id(), file_id);

        // An old prefix watermark is only a candidate. Missing manifest
        // metadata and corrupt bytes must be repaired before promotion.
        let src2 = std::env::temp_dir().join("promtuz-dl-src2.bin");
        let mut bytes2 = vec![0x11u8; 300 * 1024];
        bytes2[wire::CHUNK_SIZE..].fill(0x22);
        std::fs::write(&src2, &bytes2).unwrap();
        let (file_id2, _) = prepare_send(src2.to_str().unwrap(), 3600).unwrap();
        offer_to(id_b.ipk, file_id2);
        store::forget_partial(&file_id2);
        let path2 =
            std::env::temp_dir().join("promtuz-resume-stored-prefix.part").display().to_string();
        std::fs::write(&path2, vec![0x99u8; wire::CHUNK_SIZE]).unwrap();
        store::partial_put(&store::Partial {
            file_id: file_id2,
            source_ipk: [1; 32],
            total: 300 * 1024,
            chunk_size: wire::CHUNK_SIZE as u32,
            manifest: None,
            have: 1,
            state: store::ACTIVE,
            path: path2.clone(),
            updated_at: 0,
        })
        .unwrap();

        pull(&link_b, file_id2, 300 * 1024, &id_b).await.unwrap();
        assert_eq!(store::partial_get(&file_id2).unwrap().state, store::DONE);
        let got = std::fs::read(&path2).unwrap();
        assert_eq!(got.len(), 300 * 1024);
        assert_eq!(got, bytes2, "unverified prefix must be re-transferred");
    }

    /// Exercise the real authenticated QUIC transfer without saving either
    /// endpoint as a contact, then revoke access while the link stays open.
    #[tokio::test]
    async fn group_media_without_pairing_is_scoped_and_revocable() {
        use crate::data::contact::Contact;
        use crate::data::conversation::Conversation;
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let mut sender_seed = [0xe8; 32];
        sender_seed[0] = 81;
        let mut receiver_seed = [0xe8; 32];
        receiver_seed[0] = 82;
        let sender = identity(sender_seed);
        let receiver = identity(receiver_seed);
        assert!(!Contact::is_paired(&sender.ipk));
        assert!(!Contact::is_paired(&receiver.ipk));
        let group = Conversation::join_group(&sender.ipk, &[sender.ipk, receiver.ipk]).unwrap();
        let (serve, receive, _ep_a, _ep_b) = linked_pair(receiver.ipk, sender.ipk).await;
        let server = tokio::spawn(serve_streams(serve, sender.clone()));
        let src = dir.join("unpaired-group-media.bin");
        let bytes = vec![0x81; 300 * 1024];
        std::fs::write(&src, &bytes).unwrap();
        let (fid, size) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        // Remove prior runs' references before establishing this test's scope.
        crate::db::messages::MESSAGES_DB
            .lock()
            .execute("DELETE FROM message_media WHERE file_id = ?1", [fid.as_slice()])
            .unwrap();
        store::forget_partial(&fid);
        offer_in(group, fid);
        assert!(offered_to(&fid, &receiver.ipk, &sender.ipk));
        pull(&receive, fid, size, &receiver).await.unwrap();
        assert_eq!(std::fs::read(store::partial_get(&fid).unwrap().path).unwrap(), bytes);
        assert!(!Contact::is_paired(&receiver.ipk), "transfer must not pair the members");

        // A file in a different conversation is inaccessible even with a
        // known hash and an authenticated connection to its sender.
        let other = Conversation::join_group(&sender.ipk, &[sender.ipk, [83; 32]]).unwrap();
        let private_src = dir.join("other-group-media.bin");
        std::fs::write(&private_src, vec![0x82; 300 * 1024]).unwrap();
        let (private_fid, private_size) =
            prepare_send(private_src.to_str().unwrap(), 3600).unwrap();
        store::forget_partial(&private_fid);
        offer_in(other, private_fid);
        assert!(pull(&receive, private_fid, private_size, &receiver).await.is_err());
        assert!(store::partial_get(&private_fid).is_none());

        // Even an old direct offer must not authorize an unpaired peer.
        offer_to(receiver.ipk, fid);
        Conversation::deactivate_member(&group, &receiver.ipk).unwrap();
        assert!(
            pull(&receive, fid, size, &receiver).await.is_err(),
            "removed member denied on existing link"
        );
        Conversation::add_member(&group, &receiver.ipk, 0).unwrap();
        Conversation::deactivate_member(&group, &sender.ipk).unwrap();
        assert!(
            pull(&receive, fid, size, &receiver).await.is_err(),
            "sender who left must stop serving"
        );
        assert!(store::retention_get(&fid).is_some(), "revocation preserves the sender's file");
        server.abort();
    }

    #[tokio::test]
    async fn serve_refuses_wrong_ipk_before_any_chunk() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id_a = paired_identity([53u8; 32]);
        let id_b = paired_identity([54u8; 32]);
        // The imposter is even a paired contact with a valid binding — only
        // its IPK differs from the peer the server expects on this link.
        let imposter = paired_identity([55u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        let src = std::env::temp_dir().join("promtuz-dl-src3.bin");
        std::fs::write(&src, vec![0x33u8; 300 * 1024]).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();

        assert!(pull(&link_b, file_id, 300 * 1024, &imposter).await.is_err());
        assert!(store::partial_get(&file_id).is_none(), "no state before auth passes");
        assert!(
            !std::path::Path::new(&store::partial_path(&file_id)).exists(),
            "no bytes before auth passes"
        );
    }

    #[tokio::test]
    async fn serve_answers_gone_to_a_peer_the_file_was_not_offered_to() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let id_a = paired_identity([71u8; 32]);
        let id_b = paired_identity([72u8; 32]);
        let other = paired_identity([73u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        let src = std::env::temp_dir().join("promtuz-dl-scope.bin");
        std::fs::write(&src, vec![0x55u8; 300 * 1024]).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(other.ipk, file_id);

        assert!(pull(&link_b, file_id, 300 * 1024, &id_b).await.is_err());
        assert!(store::retention_get(&file_id).is_some(), "still retained for its recipient");
    }

    #[tokio::test]
    async fn pull_rejects_size_that_belies_the_offer() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let id_a = paired_identity([61u8; 32]);
        let id_b = paired_identity([62u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        let src = std::env::temp_dir().join("promtuz-dl-belie.bin");
        std::fs::write(&src, vec![0x77u8; 300 * 1024]).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();
        offer_to(id_b.ipk, file_id);

        // The offer lied: claim 1KB while the manifest describes 300KB. The pull
        // must reject before allocating/writing a single .part byte.
        assert!(pull(&link_b, file_id, 1024, &id_b).await.is_err());
        assert!(store::partial_get(&file_id).is_none(), "no partial when size belies offer");
        assert!(
            !std::path::Path::new(&store::partial_path(&file_id)).exists(),
            "no bytes when the manifest belies the offer"
        );
    }
}

#[cfg(test)]
mod range_adversarial_tests;
#[cfg(test)]
mod range_tests;
