//! KeyPackage upkeep: refill, rotation and republishing the stash to our homes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::pack::Packer;
use common::utils::now_ms;
use ed25519_dalek::SigningKey;
use log::debug;
use log::warn;
use tokio_util::sync::CancellationToken;

use super::keypackage::KeyPackageStash;
use super::provider::PromtuzMlsProvider;
use crate::db::outbox::OpType;
use crate::quic::dht_client::DhtClient;
use crate::quic::relay_dht_client::RelayDhtClient;
use crate::state::core;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerOutcome {
    NoOp,
    Refilled { count: usize },
    Rotated { count: usize },
}

pub async fn run_once<C: DhtClient>(
    provider: &PromtuzMlsProvider, stash: &KeyPackageStash, ipk_signer: &SigningKey,
    dht: &C, now_ms: u64,
) -> Result<SchedulerOutcome> {
    let (outcome, records) = tick(provider, stash, ipk_signer, now_ms)?;
    publish_kp_batch(dht, &records).await;
    Ok(outcome)
}

/// What one tick mints, and the records it publishes.
fn tick(
    provider: &PromtuzMlsProvider, stash: &KeyPackageStash, ipk_signer: &SigningKey, now_ms: u64,
) -> Result<(SchedulerOutcome, Vec<KeyPackageRecord>)> {
    if stash.should_refill(now_ms) {
        stash
            .ensure_stash_full(provider, ipk_signer)
            .map_err(|e| anyhow!("ensure_stash_full: {e}"))?;
        // Full snapshot, not the delta: Publish replaces at the home.
        let recs = stash
            .unconsumed_records(now_ms)
            .map_err(|e| anyhow!("unconsumed_records: {e}"))?;
        let count = recs.len();
        let outcome =
            if count == 0 { SchedulerOutcome::NoOp } else { SchedulerOutcome::Refilled { count } };
        return Ok((outcome, recs));
    }

    if stash.should_rotate(now_ms) {
        // Publishing replaces the home's stash, which evicts the old generation.
        let recs = stash
            .rotate_periodic(provider, ipk_signer, now_ms)
            .map_err(|e| anyhow!("rotate_periodic: {e}"))?;
        let count = recs.len();
        let outcome =
            if count == 0 { SchedulerOutcome::NoOp } else { SchedulerOutcome::Rotated { count } };
        return Ok((outcome, recs));
    }

    Ok((SchedulerOutcome::NoOp, Vec::new()))
}

/// Set once a KeyPackage publish succeeds. The share screen holds back the QR until then, so a new
/// user cannot hand out a link nobody can pair with.
pub static KP_PUBLISH_READY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn kp_publish_ready() -> bool {
    KP_PUBLISH_READY.load(std::sync::atomic::Ordering::Relaxed)
}

async fn publish_kp_batch<C: DhtClient>(dht: &C, records: &[KeyPackageRecord]) {
    if records.is_empty() {
        return;
    }
    let Ok(payload) = records.ser() else { return };
    let kp_id = blake3::hash(&payload).as_bytes()[..16].to_vec();
    crate::delivery::enqueue(&kp_id, OpType::KpPublish, None, &payload);
    match dht.publish_keypackages(records).await {
        Ok(()) => {
            crate::delivery::retire(&kp_id, None);
            KP_PUBLISH_READY.store(true, std::sync::atomic::Ordering::Relaxed);
        },
        Err(e) => log::warn!("KP publish failed ({e}); left in outbox, reconciler will retry"),
    }
}

/// Republishes the stash on connect. Otherwise publishing waits for low water, and a relay that
/// lost our KeyPackages would never get them back.
pub async fn ensure_kp_published<C: DhtClient>(
    provider: &PromtuzMlsProvider, stash: &KeyPackageStash, ipk_signer: &SigningKey, dht: &C,
) {
    // Purge records peers would reject, so the fill below mints replacements.
    let purged = stash.purge_invalid_records(now_ms());
    if purged > 0 {
        log::info!("ensure_kp_published: purged {purged} stale-version KP records; re-minting");
    }
    if let Err(e) = stash.ensure_stash_full(provider, ipk_signer) {
        log::warn!("ensure_kp_published: ensure_stash_full failed: {e}");
    }
    let now = now_ms();
    match stash.unconsumed_records(now) {
        Ok(recs) => publish_kp_batch(dht, &recs).await,
        Err(e) => log::warn!("ensure_kp_published: unconsumed_records failed: {e}"),
    }
}

/// How often the KP scheduler checks for refill or rotation work.
const KP_SCHEDULER_TICK_MS: u64 = 60_000;

pub(crate) async fn run_scheduler_loop(client: Arc<RelayDhtClient>, cancel: CancellationToken) {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = core().db.mls();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let our_ipk_bytes = match crate::data::identity::Identity::get() {
        Some(i) => i.ipk(),
        None => {
            warn!("MLS scheduler: identity unavailable; loop exiting");
            return;
        },
    };
    let signing = match crate::data::identity::secret_key_signing(&our_ipk_bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!("MLS scheduler: signing key unavailable: {e}; loop exiting");
            return;
        },
    };
    // Republish on connect: the relay may have lost our KP while the local stash is still full,
    // so `should_refill` would never fire.
    crate::mls::scheduler::ensure_kp_published(&provider, &stash, &signing, client.as_ref()).await;
    run_scheduler_inner(
        &provider,
        &stash,
        &signing,
        client.as_ref(),
        Duration::from_millis(KP_SCHEDULER_TICK_MS),
        cancel,
    )
    .await;
}

async fn run_scheduler_inner<C: crate::quic::dht_client::DhtClient>(
    provider: &crate::mls::PromtuzMlsProvider, stash: &crate::mls::KeyPackageStash,
    signing: &ed25519_dalek::SigningKey, dht: &C, tick_interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        let now_ms = now_ms();
        match crate::mls::scheduler::run_once(provider, stash, signing, dht, now_ms).await {
            Ok(crate::mls::scheduler::SchedulerOutcome::NoOp) => {},
            Ok(other) => {
                debug!("MLS scheduler: {other:?}");
            },
            Err(e) => {
                warn!("MLS scheduler tick failed: {e}");
            },
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("MLS scheduler: cancelled, exiting");
                return;
            }
            _ = tokio::time::sleep(tick_interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use common::proto::mls_wire::KP_SCHEDULED_ROTATION_MS;
    use common::proto::mls_wire::KP_STASH_TARGET;

    use super::*;
    use crate::test_support::mls::Party;

    /// An aged stash rotates once, and the next tick finds the new generation fresh: the minute
    /// scheduler once minted a full batch on every tick.
    #[test]
    fn aged_stash_triggers_rotation() {
        let party = Party::new(0xAA);
        let stash = KeyPackageStash::new(party.db.clone());
        stash.ensure_stash_full(&party.provider, &party.identity).unwrap();
        // The stash as it stands a week after minting at time 100.
        let sql = "UPDATE mls_keypackage_stash SET generated_at_ms = 100";
        party.db.lock().execute(sql, []).unwrap();
        let due = 100 + KP_SCHEDULED_ROTATION_MS;
        let at = |now| {
            let (outcome, records) = tick(&party.provider, &stash, &party.identity, now).unwrap();
            (outcome, records.len())
        };
        assert_eq!(at(due - 1), (SchedulerOutcome::NoOp, 0));
        let rotated = SchedulerOutcome::Rotated { count: KP_STASH_TARGET };
        assert_eq!(at(due), (rotated, KP_STASH_TARGET));
        assert_eq!(at(due), (SchedulerOutcome::NoOp, 0), "the new generation is not due");
    }
}
