use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use common::utils::now_secs;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::CHUNK_DEADLINE;
use super::CONTROL_TIMEOUT;
use super::Failure;
use super::FailureKind;
use super::TransferSend;
use super::auth;
use super::bounded;
use super::expect_fin;
use super::ranges;
use super::remote_failure;
use super::report_failure;
use super::sharing;
use super::store;
use super::v2;
use super::wire;
use crate::p2p::diagnostics;
use crate::p2p::diagnostics::Event;
use crate::state::Core;

const WAKE_BACKOFF_SECS: u64 = 60;
pub(super) const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
const RETRY_COOLDOWN_SECS: u64 = 60;
static PULLING: Semaphore = Semaphore::const_new(4);

fn protocol_failure(message: &'static str) -> Failure {
    Failure::new(FailureKind::InvalidData, wire::InvalidFrame(message.into()))
}

/// Pulls in flight: two `download`s of one file must not co-write its partial; the loser no-ops.
pub(super) static DOWNLOADING: LazyLock<Mutex<HashSet<[u8; 32]>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub(super) struct PullGuard(pub(super) [u8; 32]);
impl Drop for PullGuard {
    fn drop(&mut self) {
        DOWNLOADING.lock().remove(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadTrigger {
    Requested,
    Reconnect,
    NetworkChange,
    WakeResponse,
}

pub(super) async fn download_with_policy(
    c: &'static Core, file_id: [u8; 32], trigger: DownloadTrigger,
) -> anyhow::Result<()> {
    if !DOWNLOADING.lock().insert(file_id) {
        return Ok(());
    }
    let _guard = PullGuard(file_id);
    // Recheck after owning the writer slot, since another worker may have just finished. Only a
    // requested download retries a FAILED file.
    let current = store::partial_get_tx(&c.db.transfers().lock(), &file_id);
    if current.as_ref().is_some_and(|p| p.is_complete()) {
        return Ok(());
    }
    if matches!(trigger, DownloadTrigger::Reconnect | DownloadTrigger::NetworkChange) {
        if current.as_ref().is_none_or(|p| p.state == store::FAILED) {
            return Ok(());
        }
        if trigger == DownloadTrigger::Reconnect
            && store::retry_after_tx(&c.db.transfers().lock(), &file_id) > now_secs()
        {
            return Ok(());
        }
    }
    if trigger == DownloadTrigger::WakeResponse
        && !store::claim_ready_retry_tx(&c.db.transfers().lock(), &file_id)?
    {
        return Ok(());
    }
    // Register before looking up the message: deletion before registration is
    // caught by the lookup, deletion after it cancels this generation.
    let lease = store::receiver_lease(&c.db, file_id);
    let _slot = tokio::select! {
        slot = PULLING.acquire() => slot?,
        _ = lease.cancel.cancelled() => return Ok(()),
    };
    let offer = crate::data::media::attachment_offer_tx(&c.db.messages().lock(), &file_id)?;
    let Some((peer, offered_size)) = offer else {
        if lease.cancel.is_cancelled() {
            return Ok(());
        }
        // No incoming provider does not imply no holder: an outgoing message
        // or staged composer may still own this content-addressed file.
        let partial = store::partial_get_tx(&c.db.transfers().lock(), &file_id);
        if let Some(p) = partial {
            set_state(c, &file_id, p.source_ipk, store::FAILED, &lease)?;
        }
        crate::data::media::unlink_orphaned(&c.db, &c.db.messages().lock(), &[file_id]);
        anyhow::bail!("no incoming offer for that file_id");
    };
    let local = match auth::local_auth() {
        Ok(local) => local,
        Err(e) => {
            set_state(c, &file_id, peer, store::FAILED, &lease)?;
            report_failure(FailureKind::Authentication);
            return Err(e);
        },
    };
    let helpers = sharing::candidates(c, &file_id, &peer, &local.ipk)?;
    let original = drive_download_inner(c, file_id, peer, offered_size, &local, &lease,
        &RETRY_DELAYS, !helpers.is_empty(),
        || async { crate::p2p::link(peer).await.map_err(Failure::wire) }).await;
    let needs_help = matches!(original, Ok(true)) || original.as_ref().is_err_and(|e|
        e.downcast_ref::<Failure>().is_some_and(|e| e.kind == FailureKind::Unavailable));
    let wake_original = matches!(original, Ok(true));
    let helped = if needs_help {
        try_helpers(c, file_id, offered_size, &local, &lease, helpers.clone(), |peer| async move {
            bounded(Duration::from_secs(30), crate::p2p::link(peer)).await
        }).await?
    } else { false };
    let helpers_remain = helpers.iter().any(|(provider, grant)|
        sharing::permitted(c, grant, &file_id, &local.ipk, provider).is_some());
    let held = if helped { false } else if needs_help && helpers_remain {
        // Helpers with live grants may come back online, so this is HELD, not FAILED.
        set_state(c, &file_id, peer, store::HELD, &lease)?;
        let until = now_secs() + RETRY_COOLDOWN_SECS;
        store::defer_retry_tx(&c.db.transfers().lock(), &file_id, until)?;
        true
    } else {
        if needs_help {
            set_state(c, &file_id, peer, if original.is_ok() { store::HELD } else { store::FAILED }, &lease)?;
        }
        original?
    };
    // Release the writer before sending FileWant. A fast dial-back can now
    // start its pull immediately instead of losing on_link_ready to our guard.
    drop(_slot);
    drop(lease);
    drop(_guard);
    if held
        && wake_original
        && trigger != DownloadTrigger::WakeResponse
        && store::claim_wake_tx(
            &mut c.db.transfers().lock(),
            &file_id,
            now_secs(),
            WAKE_BACKOFF_SECS,
        )?
    {
        let paired = crate::data::contact::Contact::is_paired_tx(&c.db.contacts().lock(), &peer);
        let me = crate::data::identity::Identity::local_ipk_tx(&c.db.identity().lock());
        let conversation = me.and_then(|me| {
            let db = c.db.messages().lock();
            crate::data::conversation::Conversation::for_peer_transport_tx(&db, &me, &peer, paired)
        });
        if let Some(conversation) = conversation {
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

pub(super) async fn drive_download_inner<F, Fut>(
    c: &'static Core, file_id: [u8; 32], peer: [u8; 32], offered_size: u64, local: &wire::Auth,
    lease: &store::ReceiverLease, retry_delays: &[Duration], may_help: bool, mut connect: F,
) -> anyhow::Result<bool>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<crate::p2p::PeerLink, Failure>>,
{
    for attempt in 0..=retry_delays.len() {
        if lease.cancel.is_cancelled() {
            return Ok(false);
        }
        let consent = crate::p2p::consent::may_connect_in(&c.db, &peer);
        if consent == crate::p2p::consent::Decision::No {
            set_state(c, &file_id, peer, store::FAILED, &lease)?;
            report_failure(FailureKind::Unavailable);
            anyhow::bail!("attachment sender is no longer permitted");
        }
        set_state(c, &file_id, peer, store::CONNECTING, &lease)?;
        let attempt_result = tokio::select! {
            _ = lease.cancel.cancelled() => return Ok(false),
            result = async {
                let link = connect().await?;
                let result = pull_live(c, &link, file_id, offered_size, &local, &lease).await;
                if result.as_ref().is_err_and(|e|
                    matches!(e.kind, FailureKind::Transport | FailureKind::Authentication)
                        && e.source.downcast_ref::<v2::ErrorCode>() != Some(&v2::ErrorCode::Busy)
                ) {
                    // Close only this connection, as another worker may have a newer one. Busy is
                    // an explicit answer, not a broken connection.
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
                if !(may_help && e.kind == FailureKind::Unavailable) {
                    report_failure(e.kind);
                    set_state(c, &file_id, peer, store::FAILED, &lease)?;
                }
                return Err(e.into());
            },
            Err(e) => {
                log::debug!("transfer: transient failure, attempt {}: {e}", attempt + 1);
                set_state(c, &file_id, peer, store::HELD, &lease)?;
                if let Some(delay) = retry_delays.get(attempt) {
                    diagnostics::record(Event::TransportRetry);
                    tokio::select! {
                        _ = lease.cancel.cancelled() => return Ok(false),
                        _ = tokio::time::sleep(*delay) => {},
                    }
                } else {
                    diagnostics::record(Event::RetryExhausted);
                    let until = now_secs() + RETRY_COOLDOWN_SECS;
                    store::defer_retry_tx(&c.db.transfers().lock(), &file_id, until)?;
                }
            },
        }
    }
    Ok(true)
}

/// A provider's error skips that provider and keeps verified chunks; a local disk error or a
/// cancellation stops the receiver.
pub(super) async fn try_helpers<F, Fut>(
    c: &'static Core, file: [u8; 32], size: u64, local: &wire::Auth, lease: &store::ReceiverLease,
    candidates: Vec<([u8; 32], [u8; 32])>, mut connect: F,
) -> anyhow::Result<bool>
where
    F: FnMut([u8; 32]) -> Fut,
    Fut: std::future::Future<Output = Result<crate::p2p::PeerLink, Failure>>,
{
    for (peer, grant) in candidates.into_iter().take(sharing::MAX_PROVIDERS) {
        if lease.cancel.is_cancelled() { return Ok(true); }
        if sharing::permitted(c, &grant, &file, &local.ipk, &peer).is_none() { continue; }
        set_state(c, &file, peer, store::CONNECTING, lease)?;
        let result = tokio::select! {
            biased;
            _ = lease.cancel.cancelled() => return Ok(true),
            result = async {
                let link = connect(peer).await?;
                let result = pull_v2_from(c, &link, file, size, local, lease, Some(grant)).await;
                if result.as_ref().is_err_and(|e| matches!(e.kind, FailureKind::Authentication | FailureKind::InvalidData)
                    && e.source.downcast_ref::<v2::ErrorCode>() != Some(&v2::ErrorCode::Unsupported)) {
                    link.conn.close(0u32.into(), b"invalid attachment provider");
                }
                result
            } => result,
        };
        match result {
            Ok(()) => { diagnostics::record(Event::TransferComplete); return Ok(true); },
            Err(e) if e.kind == FailureKind::Cancelled => return Ok(true),
            Err(e) if e.kind == FailureKind::Storage && !e.source.is::<v2::ErrorCode>() => {
                set_state(c, &file, peer, store::FAILED, lease)?;
                report_failure(e.kind);
                return Err(e.into());
            },
            Err(e) => log::debug!("transfer: helper unavailable: {e}"),
        }
    }
    Ok(false)
}

/// Update progress without replacing independently persisted wake/retry history.
pub(super) fn set_state(
    c: &Core, file_id: &[u8; 32], peer: [u8; 32], state: u8, lease: &store::ReceiverLease,
) -> anyhow::Result<()> {
    let current = store::partial_get_tx(&c.db.transfers().lock(), file_id);
    let mut p = current.unwrap_or(store::Partial {
        file_id: *file_id,
        source_ipk: peer,
        total: 0,
        chunk_size: 0,
        manifest: None,
        have: 0,
        state,
        path: store::partial_path(&c.db, file_id),
        updated_at: 0,
    });
    p.state = state;
    p.source_ipk = peer;
    p.updated_at = now_secs();
    store::partial_put_live_tx(&c.db.transfers().lock(), &p, lease)
}

pub(super) async fn pull_live(
    c: &'static Core, link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64,
    local: &wire::Auth, lease: &store::ReceiverLease,
) -> Result<(), Failure> {
    tokio::select! {
        biased;
        _ = lease.cancel.cancelled() => Err(Failure::new(FailureKind::Cancelled, store::Cancelled)),
        result = pull_v2_from(c, link, file_id, offered_size, local, lease, None) => result,
    }
}

fn coordinator_failure(error: anyhow::Error) -> Failure {
    if error.is::<wire::InvalidFrame>() { Failure::wire(error) } else { Failure::storage(error) }
}

pub(super) async fn open_v2_request(
    link: &crate::p2p::PeerLink, local: &wire::Auth,
) -> Result<(TransferSend, quinn::RecvStream, v2::Limits), Failure> {
    let (s, mut r) = bounded(CONTROL_TIMEOUT, link.open_stream()).await?;
    let mut s = TransferSend::new(s);
    bounded(CONTROL_TIMEOUT, auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local)).await?;
    let limits = bounded(CONTROL_TIMEOUT, v2::exchange_hello(&mut s, &mut r)).await?;
    Ok((s, r, limits))
}

async fn pull_v2_from(
    c: &'static Core, link: &crate::p2p::PeerLink, file_id: [u8; 32], offered_size: u64,
    local: &wire::Auth, lease: &store::ReceiverLease, grant: Option<[u8; 32]>,
) -> Result<(), Failure> {
    let (mut s, mut r, limits) = open_v2_request(link, local).await?;
    if grant.is_some() && !limits.sharing { return Err(remote_failure(v2::ErrorCode::Unsupported)); }
    let describe = match grant {
        Some(grant) => v2::Frame::DescribeShared { file_id, grant },
        None => v2::Frame::Describe { file_id },
    };
    bounded(CONTROL_TIMEOUT, v2::write_frame(&mut s, &describe)).await?;
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
        ranges::Receiver::open_async(c, file_id, link.ipk, manifest.clone(), offered_size, lease)
            .await
            .map_err(coordinator_failure)?;
    while receiver.prefix() < manifest.chunks.len() as u32 {
        let (mut s, mut r, limits) = open_v2_request(link, local).await?;
        if let Some(id) = &grant {
            if !limits.sharing || sharing::permitted(c, id, &file_id, &local.ipk, &link.ipk).is_none() {
                return Err(remote_failure(v2::ErrorCode::Unavailable));
            }
        }
        let requested = receiver.missing(limits.max_ranges as usize, limits.max_chunks as u32);
        let expected = v2::validate_ranges(&requested, &manifest, limits).map_err(Failure::wire)?;
        let mut pending: Vec<u32> =
            requested.iter().flat_map(|range| range.start..range.end).collect();
        pending.reverse();
        let request = match grant {
            Some(grant) => v2::Frame::PullShared { file_id, grant, ranges: requested },
            None => v2::Frame::Pull { file_id, ranges: requested },
        };
        bounded(CONTROL_TIMEOUT, v2::write_frame(&mut s, &request)).await?;
        s.finish().map_err(|e| Failure::wire(e.into()))?;
        while let Some(expected_index) = pending.pop() {
            match bounded(CHUNK_DEADLINE, v2::read_frame_for(&mut r, v2::ReadPhase::Chunk)).await? {
                v2::Frame::Chunk { index, bytes } if index == expected_index => {
                    if grant.as_ref().is_some_and(|id| sharing::permitted(c, id, &file_id, &local.ipk, &link.ipk).is_none()) {
                        return Err(remote_failure(v2::ErrorCode::Unavailable));
                    }
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
    if grant.as_ref().is_some_and(|id| sharing::permitted(c, id, &file_id, &local.ipk, &link.ipk).is_none()) {
        return Err(remote_failure(v2::ErrorCode::Unavailable));
    }
    receiver.finish(lease).map_err(coordinator_failure)?;
    store::defer_retry_tx(&c.db.transfers().lock(), &file_id, 0).map_err(Failure::storage)?;
    Ok(())
}
