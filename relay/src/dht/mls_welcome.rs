//! MLS Welcome envelope queue at the home relay.
//!
//! Owns the `dht_welcome` keyspace plus the three RPC
//! handlers wired into [`super::handler::handle_dht_request`]:
//!
//! - [`handle_welcome_publish`] — sender-relay deposits a fresh
//!   `WelcomeEnvelopeP` for an offline recipient.
//! - [`handle_welcome_fetch`]   — recipient-relay drains the
//!   recipient's queued welcomes (signed by the user's IPK,
//!   `requester_relay_id`-bound).
//! - [`handle_welcome_ack`]     — recipient-relay deletes processed
//!   welcomes by id.
//!
//! ## Why a separate keyspace (vs. piggybacking on `dht_queue`)?
//!
//! Welcome envelopes have:
//! - a longer retention window ([`WELCOME_LIFETIME_MS = 30 d`]) than
//!   regular application messages — a user added to a group while
//!   offline for 25 days must still receive the invitation on
//!   reconnect.
//! - a different per-recipient cap policy
//!   ([`MAX_WELCOMES_PER_RECIPIENT = 32`]); group invites are rare,
//!   so we can afford to keep more pending without blowing the queue.
//!
//! Splitting keyspaces lets the per-keyspace eviction policies diverge
//! cleanly and preserves the "Welcomes-first on drain" ordering without
//! interleaving them with application traffic.
//!
//! ## Storage layout
//!
//! ```text
//!   stash_prefix(32) || welcome_id(8)   = 40 bytes
//!   stash_prefix    = BLAKE3("welcome:" || recipient_ipk)
//!   welcome_id      = 8 random bytes minted by the home at store time
//! ```
//!
//! fjall's exact `prefix()` walks all welcomes for a single recipient by
//! the 32-byte `stash_prefix` (same idiom as `dht_keypackage`).
//!
//! Random `welcome_id` is the home's responsibility — the publishing
//! sender does *not* mint it. Two senders publishing the same envelope
//! to the same home thus create two distinct rows; the recipient's
//! libcore dedupes by the inner Welcome's group_id + kp_ref_used at
//! decrypt time (openmls rejects a duplicate welcome trying to re-
//! consume an already-consumed KP, which is the natural deduper).
//!
//! ## Authentication ladder
//!
//! - **Publish**: the home verifies the *envelope's* `sender_sig`
//!   under `sender_ipk` over [`welcome_envelope_signing_input`]. There
//!   is no extra publisher-relay user-layer signature here — the
//!   envelope sig is sufficient because it binds (group_id,
//!   sender_ipk, recipient_ipk, kp_ref_used, blob hash) under the
//!   inviter's IPK. A relay forwarding cannot forge.
//! - **Fetch**: the user-layer `user_sig` covers `(user_ipk,
//!   requester_relay_id, timestamp)`; the home additionally verifies
//!   `requester_relay_id == authenticated_peer_id` from `DhtHello`
//!   (cross-relay-replay defence, mirrors `QueueFetch`'s
//!   requester-binding check).
//! - **Ack**: same shape as Fetch, distinct domain string so a
//!   captured fetch sig can't be replayed as an ack.
//!
//! ## Lock contract
//!
//! All fjall I/O is synchronous; no `await` in this module. We
//! hold no `parking_lot` guards in the hot path. The per-relay rate
//! limiter is `governor`-backed (lock-free DashMap state).
//!

use std::num::NonZeroU32;
use std::sync::Arc;

use common::proto::mls_wire::MAX_KP_SKEW_MS;
use common::proto::mls_wire::MAX_WELCOMES_PER_RECIPIENT;
use common::proto::mls_wire::MAX_WELCOME_ACK_IDS;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WELCOME_ID_LEN;
use common::proto::mls_wire::WELCOME_LIFETIME_MS;
use common::proto::mls_wire::WelcomeAckReq;
use common::proto::mls_wire::WelcomeAckResp;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomeFetchFound;
use common::proto::mls_wire::WelcomeFetchOutcome;
use common::proto::mls_wire::WelcomeFetchReq;
use common::proto::mls_wire::WelcomeFetchResp;
use common::proto::mls_wire::WelcomePublishOutcome;
use common::proto::mls_wire::WelcomePublishReq;
use common::proto::mls_wire::WelcomePublishResp;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_envelope_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use rand::TryRng;
use rand::rngs::SysRng;

use super::Dht;

/// Length of the BLAKE3 stash-prefix, in bytes (the `prefix()` seek length
/// for per-recipient welcome scans).
const STASH_PREFIX_LEN: usize = 32;

/// Fixed on-disk key length: `stash_prefix(32) || welcome_id(8)`.
const STORAGE_KEY_LEN: usize = STASH_PREFIX_LEN + WELCOME_ID_LEN;

/// Per-relay quota for welcome publishes/fetches/acks. Same magnitude
/// as the KP fetch quota (60/h) — group invites and drains are rare.
/// Distinct from [`super::rate_limit::PerPeerLimiters`] (the bulk
/// bucket is generous; this is the welcome-specific bulkhead).
const MAX_WELCOME_RPC_PER_HOUR: u32 = 240;

// ---------------------------------------------------------------------------
// Helpers — key construction
// ---------------------------------------------------------------------------

/// Compute the 32-byte stash prefix for a recipient `ipk`.
///
/// `BLAKE3("welcome:" || ipk)`. The literal eight-byte `"welcome:"`
/// prefix differentiates welcome routing from KP-stash routing
/// (`"kp:"`) and presence routing (bare `ipk`). All three live in the
/// same DHT keyspace; the prefix ensures stash CFs don't share a hash.
///
/// One-line wrapper that defers to the canonical
/// [`super::key_helpers::stash_prefix`] helper.
pub fn stash_prefix(ipk: &[u8; 32]) -> [u8; STASH_PREFIX_LEN] {
    super::key_helpers::stash_prefix(b"welcome:", ipk)
}

/// Compose the `(stash_prefix || welcome_id)` storage key.
fn storage_key(ipk: &[u8; 32], welcome_id: &[u8; WELCOME_ID_LEN]) -> [u8; STORAGE_KEY_LEN] {
    let mut k = [0u8; STORAGE_KEY_LEN];
    k[..STASH_PREFIX_LEN].copy_from_slice(&stash_prefix(ipk));
    k[STASH_PREFIX_LEN..].copy_from_slice(welcome_id);
    k
}

// ---------------------------------------------------------------------------
// Per-relay rate limiter
// ---------------------------------------------------------------------------

type WelcomeRateKey = [u8; 32];
type WelcomeLimiter =
    RateLimiter<WelcomeRateKey, DefaultKeyedStateStore<WelcomeRateKey>, DefaultClock>;

/// Per-relay welcome RPC limiter. Distinct from
/// [`super::rate_limit::PerPeerLimiters`] (which classifies the welcome
/// RPCs as Bulk for the coarse first-line bulkhead) — this limiter
/// adds a per-relay welcome-specific quota so a peer that's well under
/// the bulk per-relay quota cannot still pin a single recipient's
/// welcome queue. Mirrors the [`super::mls_kp::KpFetchLimiters`]
/// pattern.
#[derive(Debug)]
pub(crate) struct WelcomeLimiters {
    limiter: WelcomeLimiter,
}

impl WelcomeLimiters {
    pub(crate) fn new() -> Self {
        let period =
            std::time::Duration::from_secs(3600 / MAX_WELCOME_RPC_PER_HOUR as u64);
        let burst = NonZeroU32::new(MAX_WELCOME_RPC_PER_HOUR).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::with_period(period)
            .expect("non-zero period per token")
            .allow_burst(burst);
        Self { limiter: RateLimiter::keyed(quota) }
    }

    /// Returns `Ok(())` if a token was consumed for `requester`,
    /// `Err(())` if the per-relay welcome quota is exhausted.
    pub(crate) fn check(&self, requester: &NodeId) -> Result<(), ()> {
        self.limiter.check_key(requester.as_bytes()).map_err(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Self-is-owner helper
// ---------------------------------------------------------------------------

/// True iff this relay is among the K closest to the *welcome stash key*
/// (`stash_prefix(recipient_ipk)`) by current routing-table view.
///
/// One-line wrapper that defers to the canonical
/// [`super::routing::self_in_top_k`] helper.
fn self_is_owner_for_recipient(dht: &Dht, recipient_ipk: &[u8; 32]) -> bool {
    super::routing::self_in_top_k(
        dht,
        &NodeId::from_bytes(stash_prefix(recipient_ipk)),
    )
}

// ---------------------------------------------------------------------------
// Envelope verification
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum WelcomeVerifyError {
    Malformed,
    BadSig,
}

/// Verify a `WelcomeEnvelopeP`'s shape and `sender_sig`.
///
/// Steps:
/// 1. Structural: `welcome_blob` non-empty and ≤
///    [`MAX_WELCOME_BYTES`].
/// 2. `sender_ipk` parses as Ed25519.
/// 3. `sender_sig` verifies under `sender_ipk` over
///    [`welcome_envelope_signing_input`].
///
/// We do *not* re-verify the inner `recipient_ipk` field against any
/// external reference here — the publish-handler already cross-checks
/// it against the `WelcomePublishReq`'s top-level recipient if the
/// caller wants to enforce match. The publish stores the welcome at the
/// K=3 homes of the recipient; `recipient_ipk` is the *only*
/// recipient-identity in this RPC, with no separate top-level recipient
/// field.
fn verify_welcome_envelope(env: &WelcomeEnvelopeP) -> Result<(), WelcomeVerifyError> {
    if env.welcome_blob.0.is_empty() {
        return Err(WelcomeVerifyError::Malformed);
    }
    if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
        return Err(WelcomeVerifyError::Malformed);
    }

    let vk = VerifyingKey::from_bytes(&env.sender_ipk.0)
        .map_err(|_| WelcomeVerifyError::Malformed)?;
    let sig = Signature::from_bytes(&env.sender_sig.0);
    let msg = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &env.group_id.0,
        &env.sender_ipk.0,
        &env.recipient_ipk.0,
        &env.kp_ref_used.0,
        &env.welcome_blob.0,
    );
    vk.verify_strict(&msg, &sig).map_err(|_| WelcomeVerifyError::BadSig)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Storage primitives — visible to tests and to the handlers
// ---------------------------------------------------------------------------

/// Iterate all stored welcome rows for a recipient (in `welcome_id`
/// byte order — opaque-deterministic, sufficient for "drain in
/// arrival-ish order"). Returns `(storage_key, envelope, expires_at_ms)`
/// triples; the expiry is decoded from the trailing 8 bytes of the
/// stored value (we prepend it to the postcard-encoded envelope at
/// store time so we can filter expired rows without deserialising the
/// full envelope first).
fn iterate_welcomes(
    dht: &Dht, ipk: &[u8; 32],
) -> Vec<([u8; STORAGE_KEY_LEN], WelcomeEnvelopeP, u64)> {
    let mut out = Vec::new();
    let prefix = stash_prefix(ipk);
    for guard in dht.store.welcome.prefix(prefix) {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => break,
        };
        if key_bytes.len() != STORAGE_KEY_LEN {
            continue;
        }
        let mut k = [0u8; STORAGE_KEY_LEN];
        k.copy_from_slice(&key_bytes);

        // Stored value layout: `expires_at_ms (BE u64) || postcard(envelope)`.
        if value.len() < 8 {
            continue;
        }
        let mut exp_buf = [0u8; 8];
        exp_buf.copy_from_slice(&value[..8]);
        let expires_at_ms = u64::from_be_bytes(exp_buf);
        let Ok(env) = WelcomeEnvelopeP::deser(&value[8..]) else {
            continue;
        };
        out.push((k, env, expires_at_ms));
    }
    out
}

/// Count current welcomes for `ipk`, bounded so we don't walk a
/// pathological queue past the cap. Returns
/// `(count_under_cap, count_at_or_over_cap)` — a binary signal,
/// matching the `enqueue_for_home` cap-check pattern.
fn welcome_count_bounded(dht: &Dht, ipk: &[u8; 32]) -> (usize, bool) {
    let prefix = stash_prefix(ipk);
    let mut count: usize = 0;
    let stop_at = MAX_WELCOMES_PER_RECIPIENT.saturating_add(1);
    for guard in dht.store.welcome.prefix(prefix) {
        if guard.key().is_err() {
            return (count, true);
        }
        count += 1;
        if count >= stop_at {
            break;
        }
    }
    (count, count >= MAX_WELCOMES_PER_RECIPIENT)
}

// ---------------------------------------------------------------------------
// Public API — handlers
// ---------------------------------------------------------------------------

/// Home-side handler for [`super::DhtRequest::WelcomePublish`].
///
/// Validation ladder:
/// 1. Skew check on `req.timestamp`.
/// 2. Per-relay welcome-RPC rate limit
///    ([`WelcomeLimiters::check`]).
/// 3. Self is in K-closest for `stash_prefix(recipient_ipk)`.
/// 4. Verify the embedded envelope's `sender_sig` via
///    [`verify_welcome_envelope`].
/// 5. Per-recipient cap check
///    ([`MAX_WELCOMES_PER_RECIPIENT`]).
/// 6. Mint random `welcome_id`, persist
///    `(stash_prefix(recipient) || welcome_id) → expires_at_ms ||
///    postcard(envelope)`.
///
/// `_authenticated_peer_id` is currently unused — the envelope's
/// `sender_sig` is the binding authority. The connection-level
/// `DhtHello` peer id authenticates the *forwarding* relay, not the
/// inviter; we accept publishes from any reachable peer relay. This
/// matches the `Forward` flow's posture, where the outer wire
/// `Forward::sig` is the relay's countersig but the inner dispatch sig
/// is what binds the user.
pub(crate) fn handle_welcome_publish(
    dht: &Arc<Dht>, req: WelcomePublishReq, authenticated_peer_id: NodeId, now_ms: u64,
) -> WelcomePublishOutcome {
    let recipient_short = hex::encode(&req.envelope.recipient_ipk.0[..4]);

    // 1. Skew check.
    let skew = now_ms.abs_diff(req.timestamp);
    if skew > MAX_KP_SKEW_MS {
        common::warn!(
            "MLS welcome_publish: skew {}ms > max {}ms for recipient={}",
            skew,
            MAX_KP_SKEW_MS,
            recipient_short
        );
        return WelcomePublishOutcome::StaleTimestamp;
    }

    // 2. Per-relay rate limit.
    if dht.welcome_limiters.check(&authenticated_peer_id).is_err() {
        common::debug!(
            "MLS welcome_publish: per-relay rate-limit hit for recipient={}",
            recipient_short
        );
        return WelcomePublishOutcome::RateLimited;
    }

    // 3. Ownership.
    if !self_is_owner_for_recipient(dht, &req.envelope.recipient_ipk.0) {
        return WelcomePublishOutcome::NotOwner;
    }

    // 4. Envelope sig verify.
    match verify_welcome_envelope(&req.envelope) {
        Ok(()) => {}
        Err(e) => {
            common::warn!(
                "MLS welcome_publish: envelope sig verify failed for recipient={}: {e:?}",
                recipient_short
            );
            return WelcomePublishOutcome::BadSig;
        }
    }

    // 5. Per-recipient cap.
    let (_, full) = welcome_count_bounded(dht, &req.envelope.recipient_ipk.0);
    if full {
        common::warn!(
            "MLS welcome_publish: queue full for recipient={}",
            recipient_short
        );
        return WelcomePublishOutcome::QueueFull;
    }

    // 6. Persist.
    let mut welcome_id = [0u8; WELCOME_ID_LEN];
    if let Err(e) = SysRng.try_fill_bytes(&mut welcome_id) {
        common::error!("MLS welcome_publish: OsRng failed: {e}");
        // OS RNG genuinely failing is a fatal-class error; surface as
        // `BadSig` (the catch-all hard-fail outcome on this RPC) so we
        // don't silently store an envelope under a zero id.
        return WelcomePublishOutcome::BadSig;
    }
    let key = storage_key(&req.envelope.recipient_ipk.0, &welcome_id);

    let envelope_bytes = match req.envelope.ser() {
        Ok(b) => b,
        Err(e) => {
            common::warn!("MLS welcome_publish: envelope encode failed: {e:?}");
            return WelcomePublishOutcome::BadSig;
        }
    };

    let expires_at_ms = now_ms.saturating_add(WELCOME_LIFETIME_MS);
    let mut value = Vec::with_capacity(8 + envelope_bytes.len());
    value.extend_from_slice(&expires_at_ms.to_be_bytes());
    value.extend_from_slice(&envelope_bytes);

    if let Err(e) = dht.store.put_sync(&dht.store.welcome, key, &value) {
        common::warn!(
            "MLS welcome_publish: fjall put failed for recipient={}: {e}",
            recipient_short
        );
        return WelcomePublishOutcome::BadSig;
    }
    common::debug!(
        "MLS welcome_publish: stored welcome for recipient={} id={}",
        recipient_short,
        hex::encode(welcome_id)
    );
    WelcomePublishOutcome::Stored
}

/// Home-side handler for [`super::DhtRequest::WelcomeFetch`].
///
/// Validation ladder:
/// 1. `req.requester_relay_id == authenticated_peer_id` — cross-relay
///    replay defence (mirrors `QueueFetch`).
/// 2. Skew check on `req.timestamp`.
/// 3. Per-relay welcome-RPC rate limit.
/// 4. Self is in K-closest for `stash_prefix(user_ipk)`.
/// 5. User-layer `user_sig` verifies under `user_ipk` over
///    [`welcome_fetch_signing_input`].
/// 6. Walk `cf_dht_welcome` for `user_ipk`, filter expired rows,
///    return at most [`MAX_WELCOMES_PER_RECIPIENT`] entries
///    (the cap is also the per-fetch ceiling — small enough that one
///    fetch always drains the queue).
///
/// **Defensive failure shape**: the `BadSig` outcome covers the
/// requester-binding mismatch case as well as the literal sig
/// failure. Both result in a refusal that doesn't leak whether the
/// recipient has welcomes — same defensive response shape as
/// `KeyPackageFetch`'s `RateLimited` outcome on the equivalent
/// branch.
pub(crate) fn handle_welcome_fetch(
    dht: &Arc<Dht>, req: WelcomeFetchReq, authenticated_peer_id: NodeId, now_ms: u64,
) -> WelcomeFetchOutcome {
    // 1. Requester binding (cross-relay replay defence).
    if req.requester_relay_id != authenticated_peer_id {
        common::warn!(
            "MLS welcome_fetch: requester binding mismatch for user_ipk={}",
            hex::encode(&req.user_ipk.0[..4])
        );
        return WelcomeFetchOutcome::BadSig;
    }

    // 2. Skew check.
    let skew = now_ms.abs_diff(req.timestamp);
    if skew > MAX_KP_SKEW_MS {
        return WelcomeFetchOutcome::BadSig;
    }

    // 3. Per-relay rate limit.
    if dht.welcome_limiters.check(&authenticated_peer_id).is_err() {
        return WelcomeFetchOutcome::RateLimited;
    }

    // 4. Ownership.
    if !self_is_owner_for_recipient(dht, &req.user_ipk.0) {
        return WelcomeFetchOutcome::NotOwner;
    }

    // 5. User sig verify.
    let Ok(vk) = VerifyingKey::from_bytes(&req.user_ipk.0) else {
        return WelcomeFetchOutcome::BadSig;
    };
    let sig = Signature::from_bytes(&req.user_sig.0);
    let msg = welcome_fetch_signing_input(
        MLS_WIRE_VERSION,
        &req.user_ipk.0,
        &req.requester_relay_id,
        req.timestamp,
    );
    if vk.verify_strict(&msg, &sig).is_err() {
        return WelcomeFetchOutcome::BadSig;
    }

    // 6. Walk + filter + collect. We also opportunistically evict
    //    expired rows on this pass — same policy as the KP fetch path.
    let entries = iterate_welcomes(dht, &req.user_ipk.0);
    let mut to_evict: Vec<[u8; STORAGE_KEY_LEN]> = Vec::new();
    let mut welcomes: Vec<WelcomeEntry> = Vec::with_capacity(entries.len());
    for (key, env, expires_at_ms) in entries {
        if expires_at_ms <= now_ms {
            to_evict.push(key);
            continue;
        }
        let mut welcome_id = [0u8; WELCOME_ID_LEN];
        welcome_id.copy_from_slice(&key[STASH_PREFIX_LEN..]);
        welcomes.push(WelcomeEntry {
            welcome_id: welcome_id.into(),
            envelope: env,
        });
        if welcomes.len() >= MAX_WELCOMES_PER_RECIPIENT {
            break;
        }
    }

    for k in &to_evict {
        let _ = dht.store.welcome.remove(k);
    }

    WelcomeFetchOutcome::Found(WelcomeFetchFound { welcomes })
}

/// Home-side handler for [`super::DhtRequest::WelcomeAck`].
///
/// Validation ladder:
/// 1. `welcome_ids.len() <= MAX_WELCOME_ACK_IDS`.
/// 2. `req.requester_relay_id == authenticated_peer_id`.
/// 3. Skew check.
/// 4. Per-relay rate limit.
/// 5. Self is in K-closest.
/// 6. User sig verifies over [`welcome_ack_signing_input`].
/// 7. Delete each `(stash_prefix(user_ipk) || welcome_id)` row.
///
/// Returns `ok = true` if the ack was processed (sig + binding ok),
/// `ok = false` otherwise. Idempotent — ids that aren't present are
/// silently no-ops.
pub(crate) fn handle_welcome_ack(
    dht: &Arc<Dht>, req: WelcomeAckReq, authenticated_peer_id: NodeId, now_ms: u64,
) -> WelcomeAckResp {
    // 1. ID-list bound.
    if req.welcome_ids.len() > MAX_WELCOME_ACK_IDS {
        return WelcomeAckResp { ok: false };
    }

    // 2. Requester binding.
    if req.requester_relay_id != authenticated_peer_id {
        return WelcomeAckResp { ok: false };
    }

    // 3. Skew check.
    let skew = now_ms.abs_diff(req.timestamp);
    if skew > MAX_KP_SKEW_MS {
        return WelcomeAckResp { ok: false };
    }

    // 4. Rate limit.
    if dht.welcome_limiters.check(&authenticated_peer_id).is_err() {
        return WelcomeAckResp { ok: false };
    }

    // 5. Ownership.
    if !self_is_owner_for_recipient(dht, &req.user_ipk.0) {
        return WelcomeAckResp { ok: false };
    }

    // 6. User sig verify.
    let Ok(vk) = VerifyingKey::from_bytes(&req.user_ipk.0) else {
        return WelcomeAckResp { ok: false };
    };
    let ids: Vec<[u8; WELCOME_ID_LEN]> =
        req.welcome_ids.iter().map(|b| b.0).collect();
    let msg = welcome_ack_signing_input(
        MLS_WIRE_VERSION,
        &req.user_ipk.0,
        &req.requester_relay_id,
        &ids,
        req.timestamp,
    );
    let sig = Signature::from_bytes(&req.user_sig.0);
    if vk.verify_strict(&msg, &sig).is_err() {
        return WelcomeAckResp { ok: false };
    }

    // 7. Delete by id.
    //
    // Any per-row delete failure surfaces as `ok: false` so the recipient
    // knows their acks weren't honoured (and re-tries on next reconnect).
    // These must not silently return `ok: true`, which would make the queue
    // grow forever from the recipient's perspective.
    let mut all_ok = true;
    for id in &ids {
        let key = storage_key(&req.user_ipk.0, id);
        if let Err(e) = dht.store.welcome.remove(key) {
            common::warn!(
                "MLS welcome_ack: delete failed for ipk={} welcome_id={}: {e}",
                hex::encode(&req.user_ipk.0[..4]),
                hex::encode(id)
            );
            all_ok = false;
        }
    }

    if all_ok {
        common::debug!(
            "MLS welcome_ack: deleted {} welcome(s) for ipk={}",
            ids.len(),
            hex::encode(&req.user_ipk.0[..4])
        );
    }
    WelcomeAckResp { ok: all_ok }
}

// ---------------------------------------------------------------------------
// CloseReason mapping helpers
// ---------------------------------------------------------------------------

/// Map a [`WelcomePublishOutcome`] to the `CloseReason` representing
/// a hard protocol violation. Soft outcomes (`Stored`, `NotOwner`,
/// `QueueFull`) surface only in the response body — same posture as
/// `Forward` / `KeyPackagePublish`.
pub(crate) fn close_reason_for_publish(
    outcome: WelcomePublishOutcome,
) -> Option<common::quic::CloseReason> {
    use common::quic::CloseReason;
    match outcome {
        WelcomePublishOutcome::BadSig => Some(CloseReason::WelcomeMalformed),
        WelcomePublishOutcome::RateLimited => Some(CloseReason::WelcomeRateLimited),
        WelcomePublishOutcome::QueueFull => Some(CloseReason::WelcomeQueueFull),
        WelcomePublishOutcome::Stored
        | WelcomePublishOutcome::NotOwner
        | WelcomePublishOutcome::StaleTimestamp => None,
    }
}

/// Map a [`WelcomeFetchOutcome`] to the corresponding `CloseReason`.
pub(crate) fn close_reason_for_fetch(
    outcome: &WelcomeFetchOutcome,
) -> Option<common::quic::CloseReason> {
    use common::quic::CloseReason;
    match outcome {
        WelcomeFetchOutcome::BadSig => Some(CloseReason::WelcomeMalformed),
        WelcomeFetchOutcome::RateLimited => Some(CloseReason::WelcomeRateLimited),
        WelcomeFetchOutcome::Found(_) | WelcomeFetchOutcome::NotOwner => None,
    }
}

/// Wrap an outcome into the response shape (mirrors mls_kp).
pub(crate) fn wrap_publish_outcome(outcome: WelcomePublishOutcome) -> WelcomePublishResp {
    WelcomePublishResp { outcome }
}

pub(crate) fn wrap_fetch_outcome(outcome: WelcomeFetchOutcome) -> WelcomeFetchResp {
    WelcomeFetchResp { outcome }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;

    /// Deterministic-distinct seed counter — same idiom as
    /// `mls_kp::tests::fresh_signing_key`.
    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        seed[16] = ((n >> 8) & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    /// Build a `Dht` with the welcome CF wired up.
    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("promtuz-mls-welcome-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
    }

    /// Build a self-consistent welcome envelope signed by `sender`
    /// for `recipient_ipk`.
    fn build_envelope(
        sender: &SigningKey, recipient_ipk: [u8; 32], blob: Vec<u8>,
    ) -> WelcomeEnvelopeP {
        let sender_ipk: [u8; 32] = sender.verifying_key().to_bytes();
        let group_id = [0xAA; 32];
        let kp_ref_used = [0xBB; 32];
        let msg = welcome_envelope_signing_input(
            MLS_WIRE_VERSION,
            &group_id,
            &sender_ipk,
            &recipient_ipk,
            &kp_ref_used,
            &blob,
        );
        let sig = sender.sign(&msg);
        WelcomeEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: group_id.into(),
            sender_ipk: sender_ipk.into(),
            recipient_ipk: recipient_ipk.into(),
            welcome_blob: blob.into(),
            kp_ref_used: kp_ref_used.into(),
            sender_sig: sig.to_bytes().into(),
        }
    }

    /// Build a fully-signed `WelcomeFetchReq`.
    fn build_fetch(
        user: &SigningKey, requester: NodeId, timestamp: u64,
    ) -> WelcomeFetchReq {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg =
            welcome_fetch_signing_input(MLS_WIRE_VERSION, &user_ipk, &requester, timestamp);
        let sig = user.sign(&msg);
        WelcomeFetchReq {
            user_ipk: user_ipk.into(),
            requester_relay_id: requester,
            timestamp,
            user_sig: sig.to_bytes().into(),
        }
    }

    /// Build a fully-signed `WelcomeAckReq`.
    fn build_ack(
        user: &SigningKey, requester: NodeId, ids: Vec<[u8; WELCOME_ID_LEN]>,
        timestamp: u64,
    ) -> WelcomeAckReq {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg = welcome_ack_signing_input(
            MLS_WIRE_VERSION,
            &user_ipk,
            &requester,
            &ids,
            timestamp,
        );
        let sig = user.sign(&msg);
        WelcomeAckReq {
            user_ipk: user_ipk.into(),
            requester_relay_id: requester,
            welcome_ids: ids.into_iter().map(|i| i.into()).collect(),
            timestamp,
            user_sig: sig.to_bytes().into(),
        }
    }

    fn fresh_now() -> u64 {
        1_700_000_000_000
    }

    // -----------------------------------------------------------------
    // 1. Publish + Fetch round-trip
    // -----------------------------------------------------------------

    #[test]
    fn publish_then_fetch_round_trip_single_welcome() {
        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let env = build_envelope(&sender, recipient_ipk, b"welcome-bytes".to_vec());
        let req = WelcomePublishReq { envelope: env.clone(), timestamp: now };
        assert_eq!(
            handle_welcome_publish(&dht, req, auth_peer, now),
            WelcomePublishOutcome::Stored
        );

        let fetch = build_fetch(&recipient, auth_peer, now);
        let outcome = handle_welcome_fetch(&dht, fetch, auth_peer, now);
        match outcome {
            WelcomeFetchOutcome::Found(found) => {
                assert_eq!(found.welcomes.len(), 1);
                assert_eq!(found.welcomes[0].envelope, env);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 2. Multiple welcomes for same recipient
    // -----------------------------------------------------------------

    #[test]
    fn publish_multiple_welcomes_for_same_recipient_returns_all_on_fetch() {
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Three different senders publish to the same recipient.
        let mut envs = Vec::new();
        for i in 0..3u8 {
            let sender = fresh_signing_key();
            let env = build_envelope(&sender, recipient_ipk, vec![i; 8]);
            envs.push(env.clone());
            let req = WelcomePublishReq { envelope: env, timestamp: now };
            assert_eq!(
                handle_welcome_publish(&dht, req, auth_peer, now),
                WelcomePublishOutcome::Stored
            );
        }

        let fetch = build_fetch(&recipient, auth_peer, now);
        match handle_welcome_fetch(&dht, fetch, auth_peer, now) {
            WelcomeFetchOutcome::Found(found) => {
                assert_eq!(found.welcomes.len(), 3);
                let blobs: std::collections::HashSet<Vec<u8>> = found
                    .welcomes
                    .iter()
                    .map(|w| w.envelope.welcome_blob.0.clone())
                    .collect();
                let expected: std::collections::HashSet<Vec<u8>> =
                    envs.iter().map(|e| e.welcome_blob.0.clone()).collect();
                assert_eq!(blobs, expected);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 3. Per-recipient queue cap
    // -----------------------------------------------------------------

    #[test]
    fn publish_beyond_per_recipient_cap_is_queue_full() {
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Publish exactly the cap. Each one consumes a rate-limit
        // token; we have a generous quota (240/h burst), so 32
        // publishes is fine.
        let sender = fresh_signing_key();
        for _ in 0..MAX_WELCOMES_PER_RECIPIENT {
            let env = build_envelope(&sender, recipient_ipk, b"x".to_vec());
            let req = WelcomePublishReq { envelope: env, timestamp: now };
            assert_eq!(
                handle_welcome_publish(&dht, req, auth_peer, now),
                WelcomePublishOutcome::Stored
            );
        }

        // The (cap+1)th publish must be QueueFull.
        let env = build_envelope(&sender, recipient_ipk, b"overflow".to_vec());
        let req = WelcomePublishReq { envelope: env, timestamp: now };
        assert_eq!(
            handle_welcome_publish(&dht, req, auth_peer, now),
            WelcomePublishOutcome::QueueFull
        );
    }

    // -----------------------------------------------------------------
    // 4. Authentication: requester binding (cross-relay replay defence)
    // -----------------------------------------------------------------

    #[test]
    fn fetch_rejects_redirected_requester() {
        // A captured `WelcomeFetch` signed for requester_a cannot be
        // replayed against the home from requester_b. Mirrors
        // mls_kp::fetch_rejects_redirected_requester.
        let user = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();

        let req_a = NodeId::new([0xAA; 32]);
        let req_b = NodeId::new([0xBB; 32]);

        let fetch = build_fetch(&user, req_a, now);
        // Authenticated peer is req_b — must reject as BadSig.
        let outcome = handle_welcome_fetch(&dht, fetch, req_b, now);
        assert!(
            matches!(outcome, WelcomeFetchOutcome::BadSig),
            "got {outcome:?}"
        );
    }

    #[test]
    fn ack_rejects_redirected_requester() {
        let user = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let req_a = NodeId::new([0xAA; 32]);
        let req_b = NodeId::new([0xBB; 32]);

        let ack = build_ack(&user, req_a, vec![[1u8; 8]], now);
        let resp = handle_welcome_ack(&dht, ack, req_b, now);
        assert!(!resp.ok);
    }

    // -----------------------------------------------------------------
    // 5. Ack deletes the welcome
    // -----------------------------------------------------------------

    #[test]
    fn ack_deletes_welcome_so_subsequent_fetch_returns_empty() {
        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let env = build_envelope(&sender, recipient_ipk, b"x".to_vec());
        assert_eq!(
            handle_welcome_publish(
                &dht,
                WelcomePublishReq { envelope: env, timestamp: now },
                auth_peer,
                now,
            ),
            WelcomePublishOutcome::Stored
        );

        let fetch_resp = handle_welcome_fetch(
            &dht,
            build_fetch(&recipient, auth_peer, now),
            auth_peer,
            now,
        );
        let ids: Vec<[u8; 8]> = match fetch_resp {
            WelcomeFetchOutcome::Found(f) => f.welcomes.iter().map(|w| w.welcome_id.0).collect(),
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(ids.len(), 1);

        let ack = build_ack(&recipient, auth_peer, ids, now);
        let resp = handle_welcome_ack(&dht, ack, auth_peer, now);
        assert!(resp.ok);

        // Second fetch should now return empty.
        let fetch_resp = handle_welcome_fetch(
            &dht,
            build_fetch(&recipient, auth_peer, now),
            auth_peer,
            now,
        );
        match fetch_resp {
            WelcomeFetchOutcome::Found(f) => assert!(f.welcomes.is_empty()),
            other => panic!("expected Found(empty), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 6. Bad sender_sig is rejected on publish
    // -----------------------------------------------------------------

    #[test]
    fn publish_with_tampered_sender_sig_is_rejected() {
        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let mut env = build_envelope(&sender, recipient_ipk, b"x".to_vec());
        env.sender_sig.0[0] ^= 0xFF;
        let req = WelcomePublishReq { envelope: env, timestamp: now };
        assert_eq!(
            handle_welcome_publish(&dht, req, auth_peer, now),
            WelcomePublishOutcome::BadSig
        );
    }

    // -----------------------------------------------------------------
    // 7. Expiry handling
    // -----------------------------------------------------------------

    #[test]
    fn fetch_silently_filters_expired_welcomes() {
        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let auth_peer = NodeId::new([0xBB; 32]);

        // Publish at t0; expires_at_ms = t0 + WELCOME_LIFETIME_MS.
        let t0 = fresh_now();
        let env = build_envelope(&sender, recipient_ipk, b"x".to_vec());
        assert_eq!(
            handle_welcome_publish(
                &dht,
                WelcomePublishReq { envelope: env, timestamp: t0 },
                auth_peer,
                t0,
            ),
            WelcomePublishOutcome::Stored
        );

        // Skip past expiry. Fetch should return Found with empty list
        // (silent filter).
        let t1 = t0 + WELCOME_LIFETIME_MS + 1;
        let fetch = build_fetch(&recipient, auth_peer, t1);
        match handle_welcome_fetch(&dht, fetch, auth_peer, t1) {
            WelcomeFetchOutcome::Found(f) => assert!(f.welcomes.is_empty()),
            other => panic!("expected Found(empty), got {other:?}"),
        }
    }

    #[test]
    fn publish_outside_skew_window_is_stale_timestamp() {
        let sender = fresh_signing_key();
        let recipient = fresh_signing_key();
        let recipient_ipk: [u8; 32] = recipient.verifying_key().to_bytes();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let auth_peer = NodeId::new([0xBB; 32]);

        let now = fresh_now();
        let env = build_envelope(&sender, recipient_ipk, b"x".to_vec());
        // Sign at `now`; verify with `now + 2 minutes` (outside 60s skew).
        let req = WelcomePublishReq { envelope: env, timestamp: now };
        let later = now + 120_000;
        assert_eq!(
            handle_welcome_publish(&dht, req, auth_peer, later),
            WelcomePublishOutcome::StaleTimestamp
        );
    }

    // -----------------------------------------------------------------
    // 8. Rate limit
    // -----------------------------------------------------------------

    #[test]
    fn rate_limit_trips_after_burst() {
        // The welcome limiter has a per-relay quota of
        // MAX_WELCOME_RPC_PER_HOUR with burst equal to it. Drain it
        // and verify subsequent fetches are RateLimited.
        let user = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        // Use the same fetch over and over; it'll always fail
        // ownership-late but the rate limit is checked *before*
        // ownership in our handler. Wait — we need to confirm: in our
        // ladder, rate-limit is step 3, ownership is step 4. But step
        // 1 (requester binding) and step 2 (skew) happen before
        // rate-limit. Both are ok here.
        let fetch = build_fetch(&user, auth_peer, now);

        // Drain the burst (240). The function still consumes a token
        // even if the eventual outcome is BadSig/NotOwner.
        let mut rate_limited_count = 0;
        for _ in 0..(MAX_WELCOME_RPC_PER_HOUR as usize + 5) {
            if handle_welcome_fetch(&dht, fetch.clone(), auth_peer, now) == WelcomeFetchOutcome::RateLimited { rate_limited_count += 1 }
        }
        assert!(
            rate_limited_count > 0,
            "rate limit must trip after the burst is exhausted"
        );
    }

    // -----------------------------------------------------------------
    // 9. Ack id list bound
    // -----------------------------------------------------------------

    #[test]
    fn ack_with_oversize_id_list_is_rejected() {
        let user = fresh_signing_key();
        let self_id = NodeId::new([0u8; 32]);
        let dht = fresh_dht(self_id);
        let now = fresh_now();
        let auth_peer = NodeId::new([0xBB; 32]);

        let mut ids: Vec<[u8; 8]> = Vec::new();
        for i in 0..(MAX_WELCOME_ACK_IDS as u32 + 1) {
            let mut id = [0u8; 8];
            id[..4].copy_from_slice(&i.to_be_bytes());
            ids.push(id);
        }
        let ack = build_ack(&user, auth_peer, ids, now);
        let resp = handle_welcome_ack(&dht, ack, auth_peer, now);
        assert!(!resp.ok);
    }

    // -----------------------------------------------------------------
    // 10. CloseReason mapping
    // -----------------------------------------------------------------

    #[test]
    fn close_reason_mapping_for_publish_outcomes() {
        use common::quic::CloseReason;
        assert!(matches!(
            close_reason_for_publish(WelcomePublishOutcome::BadSig),
            Some(CloseReason::WelcomeMalformed)
        ));
        assert!(matches!(
            close_reason_for_publish(WelcomePublishOutcome::RateLimited),
            Some(CloseReason::WelcomeRateLimited)
        ));
        assert!(matches!(
            close_reason_for_publish(WelcomePublishOutcome::QueueFull),
            Some(CloseReason::WelcomeQueueFull)
        ));
        assert!(close_reason_for_publish(WelcomePublishOutcome::Stored).is_none());
        assert!(close_reason_for_publish(WelcomePublishOutcome::NotOwner).is_none());
        assert!(
            close_reason_for_publish(WelcomePublishOutcome::StaleTimestamp).is_none()
        );
    }

    #[test]
    fn close_reason_mapping_for_fetch_outcomes() {
        use common::quic::CloseReason;
        assert!(matches!(
            close_reason_for_fetch(&WelcomeFetchOutcome::BadSig),
            Some(CloseReason::WelcomeMalformed)
        ));
        assert!(matches!(
            close_reason_for_fetch(&WelcomeFetchOutcome::RateLimited),
            Some(CloseReason::WelcomeRateLimited)
        ));
        assert!(close_reason_for_fetch(&WelcomeFetchOutcome::NotOwner).is_none());
    }

    /// Stash prefix is deterministic under the same input and changes
    /// when the IPK changes.
    #[test]
    fn stash_prefix_is_deterministic_and_change_sensitive() {
        let a = stash_prefix(&[0u8; 32]);
        let b = stash_prefix(&[0u8; 32]);
        assert_eq!(a, b);
        let c = stash_prefix(&[1u8; 32]);
        assert_ne!(a, c);
    }
}
