//! Attachment transfer: chunked-manifest pulls over a [`crate::p2p`] link, not store-and-forward.

use std::time::Duration;

use crate::p2p::diagnostics::{self, Event};
use crate::state::core;
use common::utils::now_secs;
use tokio::time::timeout;

use pull::DownloadTrigger;
use pull::download_with_policy;
use serve::offered_to;
use serve::serve_streams;

pub mod auth;
mod pull;
pub(crate) mod ranges;
mod serve;
pub(crate) mod sharing;
pub mod store;
#[cfg(test)]
mod tests;
pub(crate) mod v2;
pub mod wire;

pub const AUTO_MAX: u64 = 5 * 1024 * 1024;

const DEAD_PARTIAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(8);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_DEADLINE: Duration = Duration::from_secs(5 * 60);

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

fn remote_failure(code: v2::ErrorCode) -> Failure {
    let kind = match code {
        v2::ErrorCode::Unavailable => FailureKind::Unavailable,
        v2::ErrorCode::Busy => FailureKind::Transport,
        v2::ErrorCode::Storage => FailureKind::Storage,
        v2::ErrorCode::InvalidRequest | v2::ErrorCode::Unsupported => FailureKind::InvalidData,
    };
    Failure::new(kind, code)
}

/// quinn finishes a dropped send stream. An interrupted frame must reset instead, so a stall is
/// not mistaken for a clean but malformed EOF.
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

/// Complete is terminal: trailing bytes are a protocol error, and a missing FIN times out.
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

/// Sender retention is not aged here: its row is also the sender's own copy, so expiry only stops
/// serving.
pub fn gc(now: u64) {
    let _ = store::gc_dead_partials_tx(
        &core().db.transfers().lock(),
        now.saturating_sub(DEAD_PARTIAL_TTL_SECS),
    );
    sharing::gc(now);
}

/// At startup: a staged chip that died with the process leaves retention that nothing names.
pub fn sweep_orphaned_retention() {
    let fids = store::retention_file_ids_tx(&core().db.transfers().lock());
    crate::data::media::unlink_orphaned(&core().db, &core().db.messages().lock(), &fids);
}

pub fn should_auto_download(ipk: &[u8; 32], size: u64, on_wifi: bool) -> bool {
    on_wifi && size <= AUTO_MAX && crate::data::contact::Contact::is_paired(ipk)
}

/// Moves `path` into core's storage as the sender's copy, named by content so a resend replaces
/// it. The caller hands over a private copy, never the user's original. `hold` gets the file id
/// before the copy is retained, for its owner to name it.
pub fn prepare_send(
    path: &str, ttl_secs: u64, hold: impl FnOnce([u8; 32]),
) -> anyhow::Result<([u8; 32], u64)> {
    let m = wire::Manifest::from_file(path)?;
    let file_id = m.file_id();
    hold(file_id);
    let size = m.total_size;
    let kept = format!("{}/{}.src", core().db.files_dir("transfers"), hex::encode(file_id));
    if kept != path && std::fs::rename(path, &kept).is_err() {
        std::fs::copy(path, &kept)?;
        let _ = std::fs::remove_file(path);
    }
    if let Some(old) = store::retention_get(&file_id).filter(|r| r.path != kept) {
        let _ = std::fs::remove_file(&old.path);
    }
    let expires = now_secs() + ttl_secs;
    let manifest = postcard::to_allocvec(&m)?;
    let db = core().db.transfers().lock();
    store::retention_put_tx(&db, &file_id, &kept, size, m.chunk_size, &manifest, expires)?;
    Ok((file_id, size))
}

/// Every stream starts with the mutual [`auth`] handshake; one that fails is dropped before any
/// pull is read.
pub async fn serve_link(link: crate::p2p::PeerLink) {
    let local = match auth::local_auth() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("transfer: cannot serve without local auth: {e}");
            return;
        },
    };
    serve_streams(core(), link, local).await
}

pub async fn download(file_id: [u8; 32]) -> anyhow::Result<()> {
    download_with_policy(core(), file_id, DownloadTrigger::Requested).await
}

/// `peer` became reachable; without this, held pulls wait for our own next reconnect.
pub fn resume_for_peer(peer: [u8; 32]) {
    let files = store::incomplete_file_ids_for_tx(&core().db.transfers().lock(), &peer);
    for file_id in files {
        core().spawn(async move {
            let trigger = DownloadTrigger::Reconnect;
            if let Err(e) = download_with_policy(core(), file_id, trigger).await {
                log::warn!("transfer: resume {} failed: {e}", hex::encode(&file_id[..4]));
            }
        });
    }
}

pub fn on_link_ready(peer: [u8; 32]) {
    // One early resume after a claimed wake; otherwise the cooldown holds, so failing files do
    // not restart one another on every reconnect of the shared link.
    let files = store::incomplete_file_ids_for_tx(&core().db.transfers().lock(), &peer);
    for file_id in files {
        core().spawn(async move {
            let trigger = DownloadTrigger::WakeResponse;
            if let Err(e) = download_with_policy(core(), file_id, trigger).await {
                log::debug!("transfer: ready-link resume failed: {e}");
            }
        });
    }
}

/// A reverse wake: the peer gave up dialing us, so we open the link and their side pulls once it
/// forms.
pub fn on_file_want(peer: [u8; 32], file_id: [u8; 32]) {
    let Some(me) = crate::data::identity::Identity::local_ipk() else { return };
    if !offered_to(core(), &file_id, &peer, &me) {
        return;
    }
    log::info!(
        "transfer: FileWant from {} for {}",
        hex::encode(&peer[..4]),
        hex::encode(&file_id[..4]),
    );
    core().spawn(async move {
        if let Err(e) = crate::p2p::link(peer).await {
            log::debug!("transfer: dial back to {} failed: {e}", hex::encode(&peer[..4]));
        }
    });
}

/// On reconnect and app open. FAILED is excluded, so a real error never auto-loops.
pub async fn resume_incomplete_downloads() {
    let files = store::incomplete_file_ids_tx(&core().db.transfers().lock());
    for file_id in files {
        core().spawn(async move {
            let trigger = DownloadTrigger::Reconnect;
            if let Err(e) = download_with_policy(core(), file_id, trigger).await {
                log::warn!("transfer: resume {} failed: {e}", hex::encode(&file_id[..4]));
            }
        });
    }
}

/// A network change makes the old path's cooldown obsolete, so incomplete work gets a new bounded
/// episode. LinkReady never calls this, so it cannot replenish retry budgets.
pub(crate) async fn on_network_changed() {
    let files = store::incomplete_file_ids_tx(&core().db.transfers().lock());
    for file_id in files {
        core().spawn(async move {
            let trigger = DownloadTrigger::NetworkChange;
            if let Err(e) = download_with_policy(core(), file_id, trigger).await {
                log::debug!("transfer: network-change resume failed: {e}");
            }
        });
    }
}
