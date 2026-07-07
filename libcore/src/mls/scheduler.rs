//! KeyPackage rotation scheduler.
//!
//! # Responsibilities
//!
//! - **On reconnect**: ensure the stash is full
//!   ([`KeyPackageStash::ensure_stash_full`]) — the relay-side homes
//!   may have GC'd expired entries while we were offline.
//! - **On periodic tick** (default 1 hour): check
//!   [`KeyPackageStash::should_refill`] / [`should_rotate`] and act.
//!
//! # Why a separate module
//!
//! The stash logic lives in `mls::keypackage`; the dialer is in
//! `quic::dht_client`. The scheduler is the *coordinator* — when both
//! "you have low stash" and "the dialer is wired" are true, mint and
//! publish. Splitting it lets the scheduler grow indep of either
//! sibling's implementation details, and lets us unit-test the
//! decision logic with a fake clock + fake dialer.
//!
//! # Clock injection
//!
//! Tests pass a closure `now_ms_fn: impl Fn() -> u64` so they can pin
//! "rotation due" without wall-clock games. The default
//! [`run_once`] entry point reads `crate::utils::systime`.

#![allow(dead_code)] // The production caller is a tokio::spawn from
// `quic/server.rs`, which needs the production DhtClient wiring.

use anyhow::Result;
use anyhow::anyhow;
use ed25519_dalek::SigningKey;

use super::keypackage::KeyPackageStash;
use super::provider::PromtuzMlsProvider;
use crate::db::outbox::OpType;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::KpOutcomeFilter;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::pack::Packer;

/// Outcome of one scheduler tick. Surfaced to the caller (a UI metric
/// or log line) without exposing the internal fan-out detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerOutcome {
    /// The stash is healthy; no minting or publishing was done.
    NoOp,
    /// We minted fresh KPs and enqueued them for durable publish. A
    /// best-effort publish is attempted immediately, but the batch may
    /// still be pending in the outbox until a home Stored quorum. Useful
    /// for a metrics counter ("KP refills minted").
    Refilled { count: usize },
    /// We rotated the entire stash (anti-pinning trigger). Distinct
    /// from [`Self::Refilled`] so a UI can surface the cadence event.
    Rotated { count: usize },
}

/// Run one scheduler tick. Single entry point so the production
/// `tokio::spawn`-driven loop and the test-fixture call into the
/// same function.
///
/// Order of operations:
/// 1. If the stash is empty / under low water →
///    [`KeyPackageStash::ensure_stash_full`] mints up to
///    `KP_STASH_TARGET` records, publishes via
///    [`DhtClient::publish_keypackages`].
/// 2. Else if `should_rotate` (oldest unconsumed KP older than
///    `KP_SCHEDULED_ROTATION_MS`) → mint a full batch, publish via
///    Refill domain so a captured Publish sig from a prior cycle
///    cannot replay. The old (still-in-lifetime) records survive at
///    the home; this is the *additive* anti-pinning rotation.
/// 3. Else → NoOp.
///
/// # Errors
///
/// - `KeyPackageStashError::*` → propagated from
///   `KeyPackageStash::generate_one` / `rotate_periodic`.
/// - [`DhtClientError::*`] → propagated from the dialer.
pub async fn run_once<C: DhtClient>(
    provider: &PromtuzMlsProvider, stash: &KeyPackageStash, ipk_signer: &SigningKey,
    dht: &C, now_ms: u64,
) -> Result<SchedulerOutcome> {
    if stash.should_refill(now_ms) {
        let recs = stash
            .ensure_stash_full(provider, ipk_signer)
            .map_err(|e| anyhow!("ensure_stash_full: {e}"))?;
        if recs.is_empty() {
            return Ok(SchedulerOutcome::NoOp);
        }
        let count = recs.len();
        publish_kp_batch(dht, &recs).await;
        return Ok(SchedulerOutcome::Refilled { count });
    }

    if stash.should_rotate(now_ms) {
        let recs = stash
            .rotate_periodic(provider, ipk_signer, now_ms)
            .map_err(|e| anyhow!("rotate_periodic: {e}"))?;
        if recs.is_empty() {
            return Ok(SchedulerOutcome::NoOp);
        }
        // Refill domain — additive at the home, distinct from Publish
        // so a captured Publish sig can't replay. When both "stash
        // dipped under low-water" AND "rotation cadence elapsed" are
        // true we go through Publish (the `should_refill` branch above);
        // the pure-cadence case here is Refill.
        dht.refill_keypackages(&recs, KpOutcomeFilter::Default)
            .await
            .map_err(|e| anyhow!("refill_keypackages: {e}"))?;
        return Ok(SchedulerOutcome::Rotated { count: recs.len() });
    }

    Ok(SchedulerOutcome::NoOp)
}

/// Enqueue a KP batch to the durable outbox, then best-effort publish it.
/// On ANY publish error the op is left in the outbox for the reconciler to
/// retry on reconnect — NEVER propagated (a failed publish used to be lost
/// forever once `should_refill` went false).
async fn publish_kp_batch<C: DhtClient>(dht: &C, records: &[KeyPackageRecord]) {
    if records.is_empty() {
        return;
    }
    let Ok(payload) = records.ser() else { return };
    let kp_id = blake3::hash(&payload).as_bytes()[..16].to_vec();
    crate::delivery::enqueue(&kp_id, OpType::KpPublish, None, &payload);
    match dht.publish_keypackages(records, KpOutcomeFilter::Default).await {
        Ok(()) => crate::delivery::retire(&kp_id),
        Err(e) => log::warn!("KP publish failed ({e}); left in outbox, reconciler will retry"),
    }
}

/// Republish the client's current KP stash to the relay on connect. Publish is
/// otherwise gated on local stash low-water (`should_refill`), so a relay that
/// lost our KP (restart/wipe/eviction) would never get it back. Idempotent.
pub async fn ensure_kp_published<C: DhtClient>(
    provider: &PromtuzMlsProvider, stash: &KeyPackageStash, ipk_signer: &SigningKey, dht: &C,
) {
    // Guarantee a full stash first (mints if low — covers a fresh or migration-wiped stash).
    if let Err(e) = stash.ensure_stash_full(provider, ipk_signer) {
        log::warn!("ensure_kp_published: ensure_stash_full failed: {e}");
    }
    let now = crate::utils::systime().as_millis() as u64;
    match stash.unconsumed_records(now) {
        Ok(recs) => publish_kp_batch(dht, &recs).await,
        Err(e) => log::warn!("ensure_kp_published: unconsumed_records failed: {e}"),
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use common::proto::mls_wire::KP_SCHEDULED_ROTATION_MS;
    use common::proto::mls_wire::KP_STASH_TARGET;
    use ed25519_dalek::SigningKey;
    use parking_lot::Mutex;
    use rusqlite::Connection;
    use rusqlite::params;

    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use crate::quic::dht_client::tests::FakeDhtClient;

    fn fresh_conn() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        Arc::new(Mutex::new(conn))
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Empty stash → scheduler mints `KP_STASH_TARGET` records and
    /// publishes them via the fake dialer.
    #[tokio::test(flavor = "current_thread")]
    async fn empty_stash_triggers_refill_and_publish() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        let signer = SigningKey::from_bytes(&[0x42; 32]);
        let dht = FakeDhtClient::new_arc();

        // Stash is empty → should_refill true.
        assert!(stash.should_refill(now_ms()));

        let out = run_once(&provider, &stash, &signer, dht.as_ref(), now_ms())
            .await
            .expect("run_once");
        match out {
            SchedulerOutcome::Refilled { count } => assert_eq!(count, KP_STASH_TARGET),
            other => panic!("expected Refilled, got {other:?}"),
        }

        // The fake recorded one batch of size `KP_STASH_TARGET`.
        let batches = dht.published_kp_batches.lock();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), KP_STASH_TARGET);
    }

    /// Healthy stash → NoOp; no fan-out to the dialer.
    #[tokio::test(flavor = "current_thread")]
    async fn healthy_stash_is_noop() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        let signer = SigningKey::from_bytes(&[0x55; 32]);
        let dht = FakeDhtClient::new_arc();

        // Pre-fill the stash to target.
        let _ = stash.ensure_stash_full(&provider, &signer).expect("seed");

        let out = run_once(&provider, &stash, &signer, dht.as_ref(), now_ms())
            .await
            .expect("run_once");
        assert_eq!(out, SchedulerOutcome::NoOp);
        // Dialer was untouched.
        assert_eq!(dht.published_kp_batches.lock().len(), 0);
    }

    /// `should_rotate` triggers when the oldest unconsumed KP is
    /// older than `KP_SCHEDULED_ROTATION_MS`. We fake the clock by
    /// directly aging the row in the SQLite, then verify the
    /// scheduler mints + dialer-publishes.
    #[tokio::test(flavor = "current_thread")]
    async fn aged_stash_triggers_rotation_via_refill_domain() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = SigningKey::from_bytes(&[0xAA; 32]);
        let dht = FakeDhtClient::new_arc();

        // Seed the stash, then age every row to the past.
        let _ = stash.ensure_stash_full(&provider, &signer).expect("seed");
        {
            let c = conn.lock();
            c.execute(
                "UPDATE mls_keypackage_stash SET generated_at_ms = ?1",
                params![100i64],
            )
            .expect("age");
        }

        // "Now" past rotation cadence.
        let now = 100 + KP_SCHEDULED_ROTATION_MS;

        // should_rotate true; should_refill false (count >= low water).
        assert!(stash.should_rotate(now));
        assert!(!stash.should_refill(now));

        let out = run_once(&provider, &stash, &signer, dht.as_ref(), now)
            .await
            .expect("run_once");
        match out {
            SchedulerOutcome::Rotated { count } => assert_eq!(count, KP_STASH_TARGET),
            other => panic!("expected Rotated, got {other:?}"),
        }

        // The fake recorded a batch via the Refill path. Our fake
        // doesn't distinguish Publish from Refill — both just append
        // — but the scheduler's `published_kp_batches` is incremented
        // by either, so a single batch is recorded.
        let batches = dht.published_kp_batches.lock();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), KP_STASH_TARGET);
    }

    /// **Fake clock determinism**: scheduler with `now_ms_fn` pinned
    /// at boundary - 1 → NoOp; pinned at boundary → rotation. Same
    /// row state.
    #[tokio::test(flavor = "current_thread")]
    async fn rotation_boundary_is_inclusive() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = SigningKey::from_bytes(&[0xCC; 32]);
        let dht = FakeDhtClient::new_arc();

        let _ = stash.ensure_stash_full(&provider, &signer).expect("seed");
        {
            let c = conn.lock();
            c.execute(
                "UPDATE mls_keypackage_stash SET generated_at_ms = ?1",
                params![100i64],
            )
            .expect("age");
        }

        // Just *before* the boundary → NoOp.
        let just_before = 100 + KP_SCHEDULED_ROTATION_MS - 1;
        let out = run_once(&provider, &stash, &signer, dht.as_ref(), just_before)
            .await
            .expect("run_once");
        assert_eq!(out, SchedulerOutcome::NoOp);

        // *At* the boundary → Rotated.
        let at_boundary = 100 + KP_SCHEDULED_ROTATION_MS;
        let out = run_once(&provider, &stash, &signer, dht.as_ref(), at_boundary)
            .await
            .expect("run_once");
        match out {
            SchedulerOutcome::Rotated { count } => assert_eq!(count, KP_STASH_TARGET),
            other => panic!("expected Rotated, got {other:?}"),
        }
    }

    /// Tight loop bound check: `run_once` returns within 2s on a
    /// fresh stash.
    #[tokio::test(flavor = "current_thread")]
    async fn run_once_completes_within_test_budget() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        let signer = SigningKey::from_bytes(&[0xDD; 32]);
        let dht = FakeDhtClient::new_arc();

        let start = std::time::Instant::now();
        let _ = run_once(&provider, &stash, &signer, dht.as_ref(), now_ms())
            .await
            .expect("run_once");
        // Generous bound: 1s. In practice this is well under 200ms
        // for `KP_STASH_TARGET = 100` records on a development host.
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
