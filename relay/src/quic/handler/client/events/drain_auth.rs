//! Sticky-home — handle `CRelayPacket::DrainAuth`.
//!
//! When a user reconnects to a relay R_r that is *not* in the user's
//! K-closest set, R_r must impersonate the user when issuing
//! `QueueFetch` to the K homes. The user signs a transcript binding
//! `(self_ipk, current_relay_id, timestamp)` once on connect; R_r
//! buffers `(timestamp, sig)` and reuses the same pair across all K
//! home dials.
//!
//! The transcript domain is `DHT_QUEUE_FETCH_SIG_DOMAIN`. It does not
//! bind the home being addressed, so a single signature is
//! valid for every home in the recipient's K-closest set — libcore
//! signs once per reconnect, not once per home.
//!
//! ## Verification flow
//!
//! [`verify_drain_auth`] does three things in order:
//! 1. Bound the timestamp within ±[`MAX_DHT_HELLO_SKEW_MS`] (60 s).
//! 2. Reconstruct the transcript via [`queue_fetch_signing_input`] using
//!    the relay's own NodeId — confirms the signature was minted *for
//!    this relay*, not replayed from a connection to a different one.
//! 3. Verify the signature under the user's IPK (the user's IPK is the
//!    verifying key — there's no external pubkey lookup needed).
//!
//! The handler [`handle_drain_auth`] wraps `verify_drain_auth` for the
//! `ClientContext` storage step. On verification failure we silently
//! drop the packet (logging at debug) — a malicious client could
//! otherwise probe the verifier by forging packets.
//!
//! ## Lock contract
//!
//! `parking_lot::Mutex<Option<DrainAuth>>` on `ClientContext`. Per the
//! workspace-wide rule we never hold the guard across an `await`. The
//! handler does no I/O after taking the lock,
//! so the constraint is trivially satisfied.

use common::crypto::PublicKey;
use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::quic::id::NodeId;
use common::trace;
use ed25519_dalek::Signature;

use crate::quic::handler::client::ClientCtxHandle;
use crate::util::systime;

/// Verified, freshness-bounded user authorisation to drain queues from
/// remote homes on the user's behalf.
///
/// `Clone` because the recipient drain path snapshots a copy out of
/// the mutex-guarded `Option` to use across an `await` boundary
/// (mirrors how `pending_drain` is `mem::take`-ed in `handle_ack_drain`,
/// but here we want the original to survive a single-failure retry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DrainAuth {
    /// User-local Unix time in milliseconds at the moment of signing,
    /// the `timestamp` field of `QueueFetch`.
    pub timestamp: u64,
    /// User's Ed25519 signature over [`queue_fetch_signing_input`] using
    /// the relay's own NodeId as `requester_relay_id`. Verified on
    /// arrival — this struct is the *output* of verification and
    /// represents already-trusted bytes.
    pub sig: [u8; 64],
}

/// Why a `DrainAuth` was rejected. Pure-function variant so the relay-
/// side handler can unit-test each branch without spinning up a
/// `ClientContext`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainAuthError {
    /// `now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS`.
    StaleTimestamp,
    /// `timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS`.
    FutureTimestamp,
    /// Ed25519 verify-strict against `user_ipk` failed.
    BadSig,
}

/// Pure verification: returns `Ok(DrainAuth)` only when the freshness
/// window holds *and* the signature validates under `user_ipk` (which
/// is also the user's IPK on `ClientContext`).
///
/// Pulled out of the network handler so unit tests can exercise every
/// branch with a deterministic `now_ms` and known signing key.
pub(crate) fn verify_drain_auth(
    user_ipk: &PublicKey, relay_id: &NodeId, now_ms: u64, timestamp: u64, sig: [u8; 64],
) -> Result<DrainAuth, DrainAuthError> {
    use ed25519_dalek::Verifier;

    // 1. Freshness window. Split stale/future like `QueueFetch::verify`
    //    so a clock-drifted client gets distinct telemetry from a
    //    captured-and-replayed packet.
    if now_ms > timestamp && now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS {
        return Err(DrainAuthError::StaleTimestamp);
    }
    if timestamp > now_ms && timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
        return Err(DrainAuthError::FutureTimestamp);
    }

    // 2. Signature verification.
    let transcript = queue_fetch_signing_input(user_ipk.as_bytes(), relay_id, timestamp);
    let signature = Signature::from_bytes(&sig);
    if user_ipk.verify(&transcript, &signature).is_err() {
        return Err(DrainAuthError::BadSig);
    }

    Ok(DrainAuth { timestamp, sig })
}

/// Handle a `CRelayPacket::DrainAuth` packet. Verifies the signature
/// and timestamp, then stores `(timestamp, sig)` on `ctx.drain_auth`
/// for the recipient drain path to pick up.
///
/// **Returns** `Ok(())` on success or any verification failure; the
/// stream-level handler doesn't need to distinguish — bad auth simply
/// means the recipient drain path will fall back to local-only when
/// `DrainQueue` lands.
pub(crate) async fn handle_drain_auth(
    ctx: ClientCtxHandle, timestamp: u64, sig: [u8; 64],
) -> anyhow::Result<()> {
    let now_ms = systime().as_millis() as u64;

    // The relay's NodeId for transcript reconstruction. When the DHT
    // is enabled it's the canonical `dht.node_id`. When disabled,
    // `verify_drain_auth` is meaningless (the relay can't fan out to
    // homes anyway), so we skip verification and buffer the auth
    // unchanged — a future DHT-on upgrade in steady state then
    // re-validates on first use. This is safe because the buffered
    // auth is only read by `drain.rs`'s remote fan-out, which is
    // gated on `dht.is_some()`.
    let Some(dht) = ctx.relay.dht.as_ref() else {
        trace!(
            "DRAIN_AUTH: stored without verification (DHT disabled on this relay)"
        );
        *ctx.drain_auth.lock() = Some(DrainAuth { timestamp, sig });
        return Ok(());
    };

    let relay_id = dht.node_id;
    match verify_drain_auth(&ctx.ipk, &relay_id, now_ms, timestamp, sig) {
        Ok(auth) => {
            *ctx.drain_auth.lock() = Some(auth);
            dht.metrics.inc_drain_auth_received();
            trace!("DRAIN_AUTH: accepted (timestamp = {timestamp})");
        }
        Err(reason) => {
            dht.metrics.inc_drain_auth_rejected();
            trace!("DRAIN_AUTH: rejected — {reason:?}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Pure-function tests for `verify_drain_auth`. The
    //! `ClientContext`-state-mutating wrapper `handle_drain_auth`
    //! exercises the same code path; we test the pure helper because
    //! it doesn't require a `Connection` or `Relay` fixture (both of
    //! which need a full QUIC stack to construct).
    //!
    //! The state-mutation half (`*ctx.drain_auth.lock() = …`) is a
    //! single line of trivial code; the verification semantics are
    //! what matter.

    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::crypto::PublicKey;
    use common::proto::dht_p2p::queue_fetch_signing_input;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::DrainAuthError;
    use super::verify_drain_auth;

    /// Counter-derived signing key for deterministic-yet-unique fixtures.
    /// Mirrors the `fresh_signing_key` discipline from
    /// `dht/forward.rs::tests`.
    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    fn user_ipk_pubkey(user: &SigningKey) -> PublicKey {
        PublicKey::from_bytes(&user.verifying_key().to_bytes()).expect("ipk")
    }

    fn fresh_node_id() -> NodeId {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        NodeId::new(seed)
    }

    /// Happy path: valid sig, fresh timestamp → `Ok(DrainAuth)` whose
    /// fields equal the inputs.
    #[test]
    fn handle_drain_auth_stores_valid_sig() {
        let user = fresh_signing_key();
        let user_ipk = user_ipk_pubkey(&user);
        let relay_id = fresh_node_id();
        let now_ms: u64 = 1_700_000_000_000;
        let timestamp = now_ms;

        let msg = queue_fetch_signing_input(user_ipk.as_bytes(), &relay_id, timestamp);
        let sig = user.sign(&msg).to_bytes();

        let auth = verify_drain_auth(&user_ipk, &relay_id, now_ms, timestamp, sig)
            .expect("valid sig accepted");
        assert_eq!(auth.timestamp, timestamp);
        assert_eq!(auth.sig, sig);
    }

    /// A garbage signature must be rejected.
    #[test]
    fn handle_drain_auth_rejects_invalid_sig() {
        let user = fresh_signing_key();
        let user_ipk = user_ipk_pubkey(&user);
        let relay_id = fresh_node_id();
        let now_ms: u64 = 1_700_000_000_000;
        let timestamp = now_ms;

        let bad_sig = [0u8; 64];
        let err = verify_drain_auth(&user_ipk, &relay_id, now_ms, timestamp, bad_sig)
            .expect_err("bad sig must be rejected");
        assert_eq!(err, DrainAuthError::BadSig);
    }

    /// A signature minted for a *different* relay's NodeId must not be
    /// accepted by this relay — protects against signature replay
    /// across relays the user happens to also be authenticated on.
    #[test]
    fn handle_drain_auth_rejects_sig_minted_for_other_relay() {
        let user = fresh_signing_key();
        let user_ipk = user_ipk_pubkey(&user);
        let our_relay = fresh_node_id();
        let other_relay = fresh_node_id();
        assert_ne!(our_relay, other_relay);

        let now_ms: u64 = 1_700_000_000_000;
        let timestamp = now_ms;
        let other_msg =
            queue_fetch_signing_input(user_ipk.as_bytes(), &other_relay, timestamp);
        let sig_for_other = user.sign(&other_msg).to_bytes();

        let err =
            verify_drain_auth(&user_ipk, &our_relay, now_ms, timestamp, sig_for_other)
                .expect_err("cross-relay replay must be rejected");
        assert_eq!(err, DrainAuthError::BadSig);
    }

    /// Freshness window: a timestamp more than 60 s off in either
    /// direction is rejected with the direction-specific variant. The
    /// stale/future split drives distinct telemetry, so each row pins
    /// its exact variant rather than just `is_err`. The signature is
    /// always valid over the given timestamp, proving the rejection
    /// comes from the freshness check, not signature verification.
    #[test]
    fn handle_drain_auth_rejects_out_of_window_timestamp() {
        let user = fresh_signing_key();
        let user_ipk = user_ipk_pubkey(&user);
        let relay_id = fresh_node_id();
        let now_ms: u64 = 1_700_000_000_000;

        let cases: [(u64, DrainAuthError); 2] = [
            (now_ms - 5 * 60_000, DrainAuthError::StaleTimestamp), // 5 min past
            (now_ms + 5 * 60_000, DrainAuthError::FutureTimestamp), // 5 min future
        ];

        for (timestamp, expected) in cases {
            let msg = queue_fetch_signing_input(user_ipk.as_bytes(), &relay_id, timestamp);
            let sig = user.sign(&msg).to_bytes();

            let err = verify_drain_auth(&user_ipk, &relay_id, now_ms, timestamp, sig)
                .expect_err("out-of-window ts must be rejected");
            assert_eq!(err, expected);
        }
    }

    /// Inside-window edge: `now - timestamp == MAX_DHT_HELLO_SKEW_MS`
    /// is accepted (the verify uses strict `>`, not `>=`).
    #[test]
    fn handle_drain_auth_accepts_at_skew_boundary() {
        use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
        let user = fresh_signing_key();
        let user_ipk = user_ipk_pubkey(&user);
        let relay_id = fresh_node_id();
        let now_ms: u64 = 1_700_000_000_000;
        let timestamp = now_ms - MAX_DHT_HELLO_SKEW_MS;

        let msg = queue_fetch_signing_input(user_ipk.as_bytes(), &relay_id, timestamp);
        let sig = user.sign(&msg).to_bytes();

        let auth = verify_drain_auth(&user_ipk, &relay_id, now_ms, timestamp, sig)
            .expect("at-boundary timestamp accepted");
        assert_eq!(auth.timestamp, timestamp);
    }
}
