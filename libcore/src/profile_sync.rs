//! Encrypted latest-state profile publication and refresh.
use crate::state::core;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod crypto;
pub(crate) mod store;
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub(crate) async fn run(cancel: CancellationToken) {
    let mut full = true;
    let mut maintenance = tokio::time::interval(RETRY_INTERVAL);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    maintenance.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = async {
                if full {
                    let _update = core().profile_update.lock().await;
                    if let Err(e) = store::reconcile().await { log::warn!("PROFILE: reconciliation deferred: {e}"); }
                }
                else if let Err(e) = store::refresh_pending().await { log::debug!("PROFILE: live refresh deferred: {e}"); }
            } => {},
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = maintenance.tick() => { full = true; },
            _ = core().profile_changed.notified() => {
                // Coalesce several field edits/notifications into one latest-state pass.
                tokio::time::sleep(Duration::from_secs(1)).await;
                full = core().profile_publish.swap(false, std::sync::atomic::Ordering::AcqRel);
            },
        }
    }
}
