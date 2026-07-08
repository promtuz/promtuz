//! Libcore-side DHT-RPC dialer trait.
//!
//! # What this is
//!
//! The MLS rollout introduces three RPC families that run over the
//! relay-to-relay `peer/1` ALPN: [`KeyPackagePublish`],
//! [`KeyPackageFetch`], [`KeyPackageRefill`], plus the Welcome queue
//! triplet [`WelcomePublish`], [`WelcomeFetch`], [`WelcomeAck`].
//! These are *not* part of the existing `relay/1` client surface — the
//! relays speak them to each other, and libcore's view today (a
//! plain client) doesn't natively dial `peer/1` for arbitrary DHT
//! traffic.
//!
//! Rather than baking a libcore-as-relay shim *now* (substantial
//! surface — fresh NodeId, DhtHello signing, peer cert pinning) we
//! ship the **trait** for the dialer here so:
//!
//! 1. The send/receive logic in `api/messaging.rs` and the
//!    `KeyPackageStash` rotation scheduler can be **fully implemented**
//!    against this trait — testable in-process via a stub.
//! 2. A concrete production impl can be landed separately (either a
//!    libcore→peer/1 dialer with an ephemeral NodeKey, or a pivot to
//!    routing through the existing `relay/1` connection via new
//!    CRelayPacket variants — the trait shape is agnostic).
//!
//! # Why a trait, not a concrete dialer
//!
//! The minimum-bar tests demand "lazy-create group on first send"
//! round-trips in-process (no real network). A concrete dialer that
//! opens `peer/1` cannot satisfy that. The trait lets a fake
//! implementation drive the in-process test fixture while production
//! callers wire a real one.
//!
//! # Async-trait shape
//!
//! Rust 2024 native async-fn-in-trait is used; methods return
//! `impl Future<Output = ...>` implicitly. That ties dyn-dispatch to
//! `Box<dyn Future>` through manual desugaring, so we side-step
//! dynamic dispatch by parameterising callers on a generic
//! `<C: DhtClient>` bound. All callers are generic-monomorphised, so
//! the cost is zero compared with `Box<dyn>`.
//!

#![allow(dead_code)] // Trait surface; not all methods are wired yet.

use std::future::Future;

use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KeyPackageRefillOutcome;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomePublishOutcome;
use thiserror::Error;

/// Failure modes for any [`DhtClient`] RPC. The trait is intentionally
/// terse — every variant maps to "we couldn't satisfy the call" without
/// claiming a specific protocol-layer reason; the concrete enum lives
/// behind the dialer's chosen transport.
#[derive(Debug, Error)]
pub enum DhtClientError {
    /// The caller did not configure a backend; production returns this
    /// until the real dialer is wired.
    #[error("dht_client: no backend wired (Phase 5 owns the production dialer)")]
    NotConfigured,

    /// Quorum (`K_MIN = 2`) of K=3 homes failed to acknowledge.
    #[error("dht_client: quorum not met ({succeeded}/{wanted} homes)")]
    QuorumNotMet { succeeded: usize, wanted: usize },

    /// All hedged homes returned `NoStash` / `NotFound` for a fetch.
    #[error("dht_client: target has no published KeyPackage")]
    NoStash,

    /// Underlying transport failure (connection refused, timeout, etc.).
    #[error("dht_client: transport: {0}")]
    Transport(String),

    /// Server replied with an unexpected variant.
    #[error("dht_client: protocol mismatch: {0}")]
    Protocol(String),
}

/// Convenience alias.
pub type DhtClientResult<T> = std::result::Result<T, DhtClientError>;

/// A KeyPackage fetched on the wire plus per-home auxiliary fields.
/// Surfaced so a future cross-replica static-fields check can compare
/// across hedged responses.
#[derive(Debug, Clone)]
pub struct FetchedKeyPackage {
    pub record:      KeyPackageRecord,
    pub remaining:   u32,
    pub static_hash: [u8; 32],
}

/// Outcome of [`DhtClient::publish_welcome_to_homes`] against the
/// recipient's K-closest homes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// At least `K_MIN = 2` of the K=3 homes accepted the publish.
    Stored,
    /// Quorum not met. Caller decides whether to retry or surface to UI.
    Failed,
}

/// Optional caller-supplied filter on per-home outcomes (e.g. accept
/// `Appended` and `Stored` as success, treat `RateLimited` as a soft
/// failure to retry later). Only the [`KpOutcomeFilter::Default`]
/// variant ships today; concrete dialer impls may later add
/// finer-grained behaviour without changing the trait shape.
#[derive(Debug, Clone, Copy, Default)]
pub enum KpOutcomeFilter {
    /// Treat any `Stored` / `Appended` reply as success; everything
    /// else is failure.
    #[default]
    Default,
}

/// The DHT-RPC surface libcore needs. Rust 2024 native
/// `async fn in trait`. Callers are generic-bounded
/// (`fn foo<C: DhtClient>(c: &C, ...)`) to side-step `dyn Future`
/// boxing; the global libcore wiring picks a single concrete impl and
/// stores it as `Arc<C>`.
pub trait DhtClient: Send + Sync + 'static {
    /// Publish a fresh batch of KeyPackages to **the K=3 homes of the
    /// publisher's IPK**. Concrete impls fan out and accept on
    /// `K_MIN = 2` successes (mirrors `relay/src/dht/forward.rs`).
    ///
    /// `records` must already carry valid per-record `owner_sig`s;
    /// the caller produces them via `KeyPackageStash::generate_one`.
    fn publish_keypackages(
        &self, records: &[KeyPackageRecord], outcome_filter: KpOutcomeFilter,
    ) -> impl Future<Output = DhtClientResult<()>> + Send;

    /// Top-up the existing stash. Same K-quorum as Publish but using
    /// the Refill domain so a captured Refill cannot be replayed as
    /// a Publish.
    fn refill_keypackages(
        &self, records: &[KeyPackageRecord], outcome_filter: KpOutcomeFilter,
    ) -> impl Future<Output = DhtClientResult<()>> + Send;

    /// Fetch one of `target_ipk`'s published KeyPackages. Concrete
    /// impls hedge α=3 against the target's K homes and return the
    /// first valid response.
    fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> impl Future<Output = DhtClientResult<FetchedKeyPackage>> + Send;

    /// Publish a Welcome envelope to the **recipient's** K=3 homes.
    /// Idempotent at the recipient's side (the home generates its own
    /// `welcome_id` on store).
    fn publish_welcome_to_homes(
        &self, envelope: &WelcomeEnvelopeP,
    ) -> impl Future<Output = DhtClientResult<PublishOutcome>> + Send;

    /// Live-push a Welcome to its recipient via the Dispatch path (replaces
    /// the publish-to-stash model). Online → live; offline → queued.
    fn deliver_welcome(
        &self, envelope: &WelcomeEnvelopeP,
    ) -> impl Future<Output = DhtClientResult<()>> + Send;

    /// Drain own welcomes from the K=3 homes of the user's IPK.
    /// Concrete impls fan out a `WelcomeFetch` and merge unique
    /// `(welcome_id, envelope)` pairs (the home dedupes by
    /// `welcome_id` but a malicious replica might double-store under
    /// distinct ids — caller dedupes on `(group_id, kp_ref_used)` at
    /// the application layer).
    fn fetch_welcomes(
        &self,
    ) -> impl Future<Output = DhtClientResult<Vec<WelcomeEntry>>> + Send;

    /// Ack the supplied `welcome_ids` so the homes can GC them. Like
    /// `QueueFetchAck`, the same signed transcript is reusable across
    /// all K homes — the impl signs once and fans out.
    fn ack_welcomes(
        &self, welcome_ids: &[[u8; 8]],
    ) -> impl Future<Output = DhtClientResult<()>> + Send;
}

/// "Not-wired" stub. Returns [`DhtClientError::NotConfigured`] from
/// every method. Suitable for production startup until a real dialer
/// is wired; tests that need success/failure outcomes use
/// [`tests::FakeDhtClient`] instead.
#[derive(Debug, Default, Clone)]
pub struct NotWiredDhtClient;

impl DhtClient for NotWiredDhtClient {
    async fn publish_keypackages(
        &self, _records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        Err(DhtClientError::NotConfigured)
    }

    async fn refill_keypackages(
        &self, _records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        Err(DhtClientError::NotConfigured)
    }

    async fn fetch_keypackage_for(
        &self, _target_ipk: &[u8; 32],
    ) -> DhtClientResult<FetchedKeyPackage> {
        Err(DhtClientError::NotConfigured)
    }

    async fn publish_welcome_to_homes(
        &self, _envelope: &WelcomeEnvelopeP,
    ) -> DhtClientResult<PublishOutcome> {
        Err(DhtClientError::NotConfigured)
    }

    async fn deliver_welcome(
        &self, _envelope: &WelcomeEnvelopeP,
    ) -> DhtClientResult<()> {
        Err(DhtClientError::NotConfigured)
    }

    async fn fetch_welcomes(&self) -> DhtClientResult<Vec<WelcomeEntry>> {
        Err(DhtClientError::NotConfigured)
    }

    async fn ack_welcomes(
        &self, _welcome_ids: &[[u8; 8]],
    ) -> DhtClientResult<()> {
        Err(DhtClientError::NotConfigured)
    }
}

// ---- "phantom" referenced types so docstring linkage works ----
#[allow(dead_code)]
fn _doc_links_only(
    _p: KeyPackagePublishOutcome, _f: KeyPackageFetchOutcome,
    _r: KeyPackageRefillOutcome, _w: WelcomePublishOutcome,
) {
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    /// In-process [`DhtClient`] used by tests. Holds:
    /// - a `published_kps` map from `target_ipk → Vec<KeyPackageRecord>`
    ///   so a test can prime a peer's stash before the unit-under-test
    ///   issues a fetch.
    /// - a `welcomes_pending` Vec of seeded welcomes a `fetch_welcomes`
    ///   call will drain.
    /// - a `welcomes_published` Vec recording every welcome the unit
    ///   under test pushed (so tests assert on the side effect).
    /// - per-call audit logs of `published_kp_batches` and
    ///   `welcome_acks`.
    #[derive(Debug, Default)]
    pub struct FakeDhtClient {
        pub published_kps:          Mutex<HashMap<[u8; 32], Vec<KeyPackageRecord>>>,
        pub welcomes_pending:       Mutex<Vec<WelcomeEntry>>,
        pub welcomes_published:     Mutex<Vec<WelcomeEnvelopeP>>,
        pub published_kp_batches:   Mutex<Vec<Vec<KeyPackageRecord>>>,
        pub welcome_acks:           Mutex<Vec<Vec<[u8; 8]>>>,
        /// Optional pinned outcome for `publish_welcome_to_homes` —
        /// useful to simulate a home returning `Failed`.
        pub forced_publish_welcome: Mutex<Option<PublishOutcome>>,
    }

    impl FakeDhtClient {
        pub fn new_arc() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Pre-seed the fake's view of `target_ipk`'s published stash.
        /// The unit-under-test's `fetch_keypackage_for` call will pop
        /// the first record.
        pub fn seed_kp(&self, target_ipk: &[u8; 32], record: KeyPackageRecord) {
            self.published_kps
                .lock()
                .entry(*target_ipk)
                .or_default()
                .push(record);
        }
    }

    impl DhtClient for FakeDhtClient {
        async fn publish_keypackages(
            &self, records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
        ) -> DhtClientResult<()> {
            let mut by_owner = self.published_kps.lock();
            for r in records {
                by_owner.entry(r.ipk.0).or_default().push(r.clone());
            }
            self.published_kp_batches.lock().push(records.to_vec());
            Ok(())
        }

        async fn refill_keypackages(
            &self, records: &[KeyPackageRecord], filter: KpOutcomeFilter,
        ) -> DhtClientResult<()> {
            // Refill is additive — same effect as Publish in the fake.
            self.publish_keypackages(records, filter).await
        }

        async fn fetch_keypackage_for(
            &self, target_ipk: &[u8; 32],
        ) -> DhtClientResult<FetchedKeyPackage> {
            let mut by_owner = self.published_kps.lock();
            let entry = by_owner.entry(*target_ipk).or_default();
            if entry.is_empty() {
                return Err(DhtClientError::NoStash);
            }
            let record = entry.remove(0);
            let remaining = entry.len() as u32;
            Ok(FetchedKeyPackage {
                record,
                remaining,
                static_hash: [0u8; 32],
            })
        }

        async fn publish_welcome_to_homes(
            &self, envelope: &WelcomeEnvelopeP,
        ) -> DhtClientResult<PublishOutcome> {
            self.welcomes_published.lock().push(envelope.clone());
            // If the test pinned a forced outcome, surface that;
            // otherwise default to Stored (the happy path) and seed
            // the recipient's pending queue so a co-resident
            // `fetch_welcomes` call will find it.
            if let Some(forced) = *self.forced_publish_welcome.lock() {
                return Ok(forced);
            }
            // Use a deterministic 8-byte id derived from the
            // welcome_blob hash so re-publish replaces.
            let id_src = blake3::hash(&envelope.welcome_blob.0);
            let mut id = [0u8; 8];
            id.copy_from_slice(&id_src.as_bytes()[..8]);
            self.welcomes_pending.lock().push(WelcomeEntry {
                welcome_id: id.into(),
                envelope:   envelope.clone(),
            });
            Ok(PublishOutcome::Stored)
        }

        async fn deliver_welcome(
            &self, envelope: &WelcomeEnvelopeP,
        ) -> DhtClientResult<()> {
            // Mirror publish's side effects so fetch_welcomes round-trips work.
            self.welcomes_published.lock().push(envelope.clone());
            let id_src = blake3::hash(&envelope.welcome_blob.0);
            let mut id = [0u8; 8];
            id.copy_from_slice(&id_src.as_bytes()[..8]);
            self.welcomes_pending.lock().push(WelcomeEntry {
                welcome_id: id.into(),
                envelope:   envelope.clone(),
            });
            Ok(())
        }

        async fn fetch_welcomes(&self) -> DhtClientResult<Vec<WelcomeEntry>> {
            // Fake returns a snapshot but doesn't auto-clear; tests ack
            // explicitly via `ack_welcomes`.
            Ok(self.welcomes_pending.lock().clone())
        }

        async fn ack_welcomes(
            &self, welcome_ids: &[[u8; 8]],
        ) -> DhtClientResult<()> {
            let id_set: std::collections::HashSet<[u8; 8]> =
                welcome_ids.iter().copied().collect();
            self.welcomes_pending
                .lock()
                .retain(|w| !id_set.contains(&w.welcome_id.0));
            self.welcome_acks.lock().push(welcome_ids.to_vec());
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_welcome_records_pushed_envelope() {
        let c = FakeDhtClient::default();
        let env = WelcomeEnvelopeP {
            version:       0,
            group_id:      [0u8; 32].into(),
            sender_ipk:    [0u8; 32].into(),
            recipient_ipk: [0u8; 32].into(),
            welcome_blob:  common::types::bytes::ByteVec(vec![1, 2, 3]),
            kp_ref_used:   [0u8; 32].into(),
            sender_sig:    [0u8; 64].into(),
            pairing:       None,
        };
        c.deliver_welcome(&env).await.expect("deliver");
        assert_eq!(c.welcomes_published.lock().len(), 1, "must record the pushed welcome");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn not_wired_returns_not_configured() {
        let c = NotWiredDhtClient;
        let r = c.fetch_keypackage_for(&[0u8; 32]).await;
        assert!(matches!(r, Err(DhtClientError::NotConfigured)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_client_round_trips_keypackages() {
        use common::types::bytes::ByteVec;
        use common::types::bytes::Bytes;

        let fake = FakeDhtClient::new_arc();
        let target = [0xABu8; 32];
        let rec = KeyPackageRecord {
            ipk:           target.into(),
            kp_ref:        ByteVec(vec![1; 32]),
            kp_bytes:      ByteVec(vec![2; 16]),
            expires_at_ms: 1_000_000,
            owner_sig:     Bytes([0u8; 64]),
        };
        fake.seed_kp(&target, rec.clone());

        let out = fake.fetch_keypackage_for(&target).await.expect("fetch");
        assert_eq!(out.record, rec);
        assert_eq!(out.remaining, 0);

        // Empty after consume.
        let r = fake.fetch_keypackage_for(&target).await;
        assert!(matches!(r, Err(DhtClientError::NoStash)));
    }
}
