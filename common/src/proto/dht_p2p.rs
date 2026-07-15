//! DHT Relay-to-Relay Wire Protocol
//!
//! This module is the source of truth for the DHT relay-to-relay wire
//! protocol. It carries:
//!
//! 1. The [`PresenceRecord`] data type and its dual-signature transcripts (user_sig covering
//!    `(user_ipk, relay_id, generation)`; relay_sig covering the full record).
//! 2. The full RPC catalogue: `Ping`/`Pong`, `FindNode`/`Resp`, `FindValue`/`Resp`, `Store`/`Resp`,
//!    `Tombstone`/`Resp`, `MerkleSummary`/`Resp`, `MerkleDiff`/`Resp`, and `FetchRecord`/`Resp`.
//! 3. Length-bound constants that downstream handlers check at deserialization / construction time.
//!
//! ## Why a `DhtRequest` + `DhtResponse` split (not a single `DhtPacket`)
//!
//! We choose **separate request and response enums** plus a thin outer
//! [`DhtPacket`] wrapper because:
//!
//! - Per-RPC bi-streams: one stream carries exactly one request and one (possibly multi-frame)
//!   response, so the *direction* is implicit in the stream side. Splitting the enums means the
//!   dispatcher on each side can match exhaustively against only the variants it ever receives,
//!   instead of dynamic-checking "did the peer send a response to a question I never asked".
//! - Mirrors the exemplar in `common/src/proto/relay_res.rs` (`LifetimeP` — all packet kinds are
//!   sibling variants of one enum) but specialises it to a request/response idiom because every DHT
//!   call has exactly one of each, whereas the relay/resolver lifecycle is asymmetric.
//!
//! [`DhtPacket`] still exists as a convenience for the framing layer so a
//! future non-RPC sync mode (push, gossip, etc.) can join the same wire
//! grammar without breaking existing RPCs.
//!
//! ## Signing transcript discipline
//!
//! Every helper that builds a Ed25519 signing input mirrors the layout
//! pioneered in `common/src/proto/relay_res.rs::signing_input`:
//!
//! ```text
//!   <domain> || PROTOCOL_VERSION (BE u16) || <fields in declaration order>
//! ```
//!
//! Each transcript has its own unique domain string so a captured
//! signature for one packet kind cannot be replayed as another. Both
//! signing and verifying sides call the same helper — it is the contract
//! between them.

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::proto::RelayId;
use crate::proto::client_rel::ActivityP;
use crate::proto::client_rel::PresenceState;
use crate::types::bytes::Bytes;

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  CONSTANTS  :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Domain-separation tag for the connection-level [`DhtHello`] handshake
/// sent as the very first frame on a fresh `peer/1` connection.
///
/// Distinct from every other DHT signing-input tag (`-roam-v1`,
/// `-presence-v1`, `-tombstone-v1`) so a captured signature for a
/// presence record cannot be replayed as a connection hello and vice
/// versa. Mirrors the resolver-side
/// [`crate::proto::relay_res::RELAY_HELLO_SIG_DOMAIN`] discipline (one
/// domain string per packet kind).
pub const DHT_HELLO_SIG_DOMAIN: &[u8] = b"promtuz-dht-hello-v1";

/// Maximum permitted clock skew between the dialing relay's signed
/// `timestamp` and the receiver's local clock, in milliseconds. Anything
/// outside this window is treated as a replay or a misconfigured clock
/// and rejected with [`crate::quic::CloseReason::DhtClockSkew`].
///
/// Mirrors the resolver's `HELLO_MAX_SKEW_MS` — it applies the same
/// window to `RelayHello`/`RelayHeartbeat`,
/// and consistency across packet kinds keeps a relay's local clock-drift
/// behaviour identical against either receiver.
pub const MAX_DHT_HELLO_SKEW_MS: u64 = 60_000;

/// Replication factor `k`. Bounds [`FindNodeResp::closer`].
pub const DHT_K: usize = 3;

/// Bound on [`FindNodeResp::closer`] entry counts. Equal to the
/// replication factor `k`.
pub const MAX_FIND_NODE_RESULTS: usize = DHT_K;

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  DHT HELLO  :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Connection-level signed handshake sent as the **very first frame** on
/// a freshly-opened `peer/1` (relay-to-relay) connection. The dialing
/// relay opens a uni-stream, frames a [`DhtHello`] and shuts the stream;
/// the receiving relay verifies the signature and binds the resulting
/// [`crate::quic::id::NodeId`] to the connection for the rest of its
/// lifetime.
///
/// **Why an application-layer hello rather than mTLS?** The relay's
/// `peer/1` ALPN is currently configured `with_no_client_auth()` because
/// the same QUIC `Endpoint` also accepts `client/1` connections, and
/// clients have no certs. mTLS on `peer/1` would require either two
/// endpoints or a per-ALPN client-auth toggle (neither exists yet in
/// `quinn`'s public API). An application-layer signed hello mirrors the
/// existing relay-to-resolver pattern (see
/// [`crate::proto::relay_res::LifetimeP::RelayHello`]) and gives us
/// equivalent identity binding — the dialing relay's `NodeId` is proven
/// by Ed25519 signature against the wire transcript, and the receiver
/// can drop the connection on any failure.
///
/// **Wire layout** (field order is load-bearing — both signing and
/// verifying sides walk the [`dht_hello_signing_input`] helper which
/// visits these in declaration order):
///
/// ```text
/// DhtHello {
///   node_id:   [u8; 32],   // claimed identity = BLAKE3(pubkey)
///   pubkey:    [u8; 32],   // dialer's full Ed25519 identity pubkey
///   timestamp: u64,        // ms since epoch; ±MAX_DHT_HELLO_SKEW_MS window
///   sig:       [u8; 64],   // Ed25519 signature over the canonical transcript
/// }
/// ```
///
/// **Signed transcript** (`dht_hello_signing_input`):
/// ```text
/// DHT_HELLO_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///   || node_id (32) || pubkey (32) || timestamp (BE u64)
/// ```
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DhtHello {
    /// Dialer's stable cryptographic ID, claimed identity. The verifier
    /// also checks `BLAKE3(pubkey) == node_id` (id-binding) so an attacker
    /// cannot present a benign pubkey under a different node_id.
    pub node_id:   crate::quic::id::NodeId,
    /// Dialer's full Ed25519 identity public key. Carried alongside
    /// `node_id` because `node_id` is a BLAKE3 hash and is therefore not
    /// invertible — the verifier needs the full key to check `sig`.
    /// Same reasoning as [`crate::proto::relay_res::LifetimeP::RelayHello::pubkey`].
    pub pubkey:    Bytes<32>,
    /// Sender-local Unix time in milliseconds. Bound into the signed
    /// transcript so the receiver can reject replays outside an accepted
    /// clock-skew window ([`MAX_DHT_HELLO_SKEW_MS`]).
    pub timestamp: u64,
    /// Ed25519 signature over [`dht_hello_signing_input`]. Verified
    /// under `pubkey` using `verify_strict`, mirroring the resolver's
    /// `RelayHello` verification at
    /// `resolver/src/resolver/mod.rs::verify_signed_packet`.
    pub sig:       Bytes<64>,
}

/// Build the canonical signing transcript for [`DhtHello`].
///
/// Layout:
/// ```text
///   DHT_HELLO_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///     || node_id (32) || pubkey (32) || timestamp (BE u64)
/// ```
///
/// The transcript layout deliberately mirrors
/// [`crate::proto::relay_res::relay_hello_signing_input`] field-for-field
/// — the only differences are the domain tag (so signatures are
/// non-replayable across packet kinds) and the `timestamp` width (`u64`
/// here vs `u128` in `relay_res`; chosen for parity with the rest of
/// `dht_p2p.rs` which uses `u64` for all wall-clock fields like
/// `not_before` / `not_after` / `deleted_at`).
///
/// Both signing (dialer) and verifying (receiver) sides call this helper,
/// which makes it the byte-for-byte contract — there is no second
/// implementation to keep in sync.
pub fn dht_hello_signing_input(
    node_id: &crate::quic::id::NodeId, pubkey: &[u8; 32], timestamp: u64,
) -> Vec<u8> {
    // domain (varies) + version (2) + node_id (32) + pubkey (32) + ts (8) = 76
    // + domain bytes.
    let mut buf =
        Vec::with_capacity(DHT_HELLO_SIG_DOMAIN.len() + 2 + crate::quic::id::NodeId::LEN + 32 + 8);
    buf.extend_from_slice(DHT_HELLO_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(node_id.as_bytes());
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Reasons a [`DhtHello`] can fail the inbound verification at
/// `relay/src/dht/handler.rs::handle_peer_connection`.
///
/// Maps onto the [`crate::quic::CloseReason`]`::Dht*` variants 1:1:
/// - [`Self::IdMismatch`], [`Self::MalformedPubkey`], [`Self::BadSignature`] → `DhtBadSignature`
///   (or `DhtMalformedKey` for malformed pubkey shape — caller's choice).
/// - [`Self::ClockSkew`] → `DhtClockSkew`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DhtHelloVerifyError {
    /// `node_id != BLAKE3(pubkey)` — the dialer is presenting a pubkey
    /// that does not hash to the claimed identity.
    #[error("dht hello: node_id != BLAKE3(pubkey)")]
    IdMismatch,
    /// `pubkey` did not parse as an Ed25519 verifying key. Distinct from
    /// [`Self::BadSignature`] so callers can attribute key-shape problems
    /// separately from signature-mismatch problems (mirrors
    /// [`PresenceVerifyError::MalformedRelayPubkey`]).
    #[error("dht hello: malformed Ed25519 pubkey")]
    MalformedPubkey,
    /// `sig` did not validate under `pubkey` over the canonical
    /// transcript.
    #[error("dht hello: bad signature")]
    BadSignature,
    /// `|now_ms − timestamp| > MAX_DHT_HELLO_SKEW_MS`. Indicates either a
    /// replay outside the skew window or a misconfigured clock on the
    /// dialer.
    #[error("dht hello: stale or future timestamp (clock skew)")]
    ClockSkew,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==: CRYPTO-GATED VERIFY :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

#[cfg(feature = "crypto")]
mod verify_impl {
    use ed25519_dalek::Signature;
    use ed25519_dalek::VerifyingKey;

    use super::DhtHello;
    use super::DhtHelloVerifyError;
    use super::Forward;
    use super::ForwardVerifyError;
    use super::MAX_DHT_HELLO_SKEW_MS;
    use super::MAX_FETCH_QUEUE_ACK_IDS;
    use super::PRESENCE_LEASE_MAX_MS;
    use super::PRESENCE_STATE_MAX_SKEW_MS;
    use super::PresenceConsent;
    use super::PresenceLease;
    use super::QueueFetch;
    use super::QueueFetchAck;
    use super::QueueFetchAckVerifyError;
    use super::QueueFetchVerifyError;
    use super::RelayPresenceState;
    use super::dht_hello_signing_input;
    use super::forward_signing_input;
    use super::presence_consent_signing_input;
    use super::presence_lease_signing_input;
    use super::presence_state_signing_input;
    use super::queue_fetch_ack_signing_input;
    use super::queue_fetch_signing_input;
    use crate::quic::id::NodeId;

    impl DhtHello {
        /// Validate a [`DhtHello`] received as the first frame on an
        /// inbound `peer/1` connection. Returns `Ok(())` and binds the
        /// connection's authenticated [`NodeId`] (callers stash
        /// `self.node_id` post-success) on a clean check.
        ///
        /// Mirrors the order, semantics and failure modes of the
        /// resolver-side `verify_signed_packet`:
        ///
        /// 1. **id ↔ pubkey binding**: `BLAKE3(pubkey) == node_id`. Catches an attacker presenting
        ///    a benign pubkey under a different claimed `node_id`.
        /// 2. **Pubkey shape**: `pubkey` parses as an Ed25519 verifying key. Surfaced as
        ///    `MalformedPubkey` (distinct from `BadSignature`) so operators can distinguish
        ///    key-shape problems from sig-mismatch problems.
        /// 3. **Signature**: `sig` verifies under `pubkey` over the canonical transcript built by
        ///    [`dht_hello_signing_input`]. Uses `verify_strict` (same choice as the resolver) for
        ///    the standard small-subgroup defence.
        /// 4. **Timestamp window**: `|now_ms − timestamp| ≤ MAX_DHT_HELLO_SKEW_MS`.
        ///
        /// `now_ms` is wall-clock in milliseconds since the Unix epoch,
        /// passed in explicitly so unit tests can pin a deterministic
        /// clock.
        pub fn verify(&self, now_ms: u64) -> Result<(), DhtHelloVerifyError> {
            // 1. id-binding to pubkey. NodeId::new = BLAKE3(pubkey) — same construction every other
            //    call site uses (cf. `verify_signed_packet` and `PresenceRecord::verify`).
            let derived_id = NodeId::new(self.pubkey.as_ref());
            if derived_id != self.node_id {
                return Err(DhtHelloVerifyError::IdMismatch);
            }

            // 2. Pubkey shape.
            let vk = VerifyingKey::from_bytes(&self.pubkey.0)
                .map_err(|_| DhtHelloVerifyError::MalformedPubkey)?;

            // 3. Signature.
            let sig = Signature::from_bytes(&self.sig.0);
            let msg = dht_hello_signing_input(&self.node_id, &self.pubkey.0, self.timestamp);
            vk.verify_strict(&msg, &sig).map_err(|_| DhtHelloVerifyError::BadSignature)?;

            // 4. Timestamp freshness (replay protection).
            let skew = now_ms.abs_diff(self.timestamp);
            if skew > MAX_DHT_HELLO_SKEW_MS {
                return Err(DhtHelloVerifyError::ClockSkew);
            }

            Ok(())
        }
    }

    impl PresenceConsent {
        pub fn verify(&self, now_ms: u64) -> bool {
            if now_ms.abs_diff(self.issued_at_ms) > PRESENCE_STATE_MAX_SKEW_MS {
                return false;
            }
            let Ok(key) = VerifyingKey::from_bytes(&self.owner.0) else {
                return false;
            };
            key.verify_strict(
                &presence_consent_signing_input(
                    &self.owner.0,
                    &self.recipient.0,
                    self.version,
                    self.issued_at_ms,
                    self.granted,
                ),
                &Signature::from_bytes(&self.user_sig.0),
            )
            .is_ok()
        }
    }

    impl PresenceLease {
        pub fn verify(&self, now_ms: u64) -> bool {
            if self.expires_at_ms <= self.issued_at_ms
                || self.expires_at_ms - self.issued_at_ms > PRESENCE_LEASE_MAX_MS
                || now_ms > self.expires_at_ms
                || self.issued_at_ms > now_ms + PRESENCE_STATE_MAX_SKEW_MS
            {
                return false;
            }
            let Ok(key) = VerifyingKey::from_bytes(&self.user.0) else {
                return false;
            };
            key.verify_strict(
                &presence_lease_signing_input(
                    &self.user.0,
                    &self.relay_id,
                    self.version,
                    self.issued_at_ms,
                    self.expires_at_ms,
                ),
                &Signature::from_bytes(&self.user_sig.0),
            )
            .is_ok()
        }
    }

    impl RelayPresenceState {
        pub fn verify(&self, authenticated_relay: &NodeId, now_ms: u64) -> bool {
            self.who == self.lease.user
                && self.lease.relay_id == *authenticated_relay
                && NodeId::new(&self.relay_pubkey.0) == self.lease.relay_id
                && self.lease.verify(now_ms)
                && now_ms.abs_diff(self.observed_at_ms) <= PRESENCE_STATE_MAX_SKEW_MS
                && VerifyingKey::from_bytes(&self.relay_pubkey.0).ok().is_some_and(|key| {
                    key.verify_strict(
                        &presence_state_signing_input(self),
                        &Signature::from_bytes(&self.relay_sig.0),
                    )
                    .is_ok()
                })
        }
    }

    impl Forward {
        /// Validate the **outer** sender-relay signature on a [`Forward`]
        /// RPC plus the timestamp window.
        ///
        /// **Contract:** this method does **not** verify the embedded
        /// [`crate::proto::client_rel::DispatchP::sig`]. That signature
        /// is the user-layer end-to-end authenticator and is checked by
        /// the home relay at delivery / queue time — running it here
        /// would conflate wire-format validation with delivery-time
        /// delivery decisions, and the wire validator has no access to
        /// the recipient's session state for the latter.
        ///
        /// **Why an external `sender_relay_pubkey` argument** rather
        /// than embedding the pubkey in [`Forward`]: the home relay
        /// receives every `Forward` over a `peer/1` connection that has
        /// already passed [`DhtHello`] verification, so the peer's full
        /// Ed25519 identity pubkey is cached on the connection state
        /// keyed by `sender_relay_id`. Pulling it from there saves 32
        /// bytes per `Forward` on the wire and prevents a per-`Forward`
        /// id-binding check (the [`DhtHello`] already proved
        /// `BLAKE3(sender_relay_pubkey) == sender_relay_id`). The
        /// home-relay handler is the call-site that supplies this
        /// argument from `Dht::peer_conns`.
        ///
        /// Steps:
        /// 1. **Pubkey shape**: caller's `sender_relay_pubkey` parses as Ed25519. Surfaced as
        ///    `MalformedField`.
        /// 2. **Signature**: `sig` verifies under `sender_relay_pubkey` over
        ///    [`forward_signing_input`]. Uses `verify_strict` for small-subgroup defence (mirrors
        ///    [`DhtHello::verify`]).
        /// 3. **Timestamp window**: `|now_ms − timestamp| ≤ MAX_DHT_HELLO_SKEW_MS`. Stale and
        ///    future skew surface as distinct `StaleTimestamp` / `FutureTimestamp` error variants
        ///    so the home relay can log them separately.
        pub fn verify(
            &self, sender_relay_pubkey: &[u8; 32], now_ms: u64,
        ) -> Result<(), ForwardVerifyError> {
            // 1. Pubkey shape.
            let vk = VerifyingKey::from_bytes(sender_relay_pubkey)
                .map_err(|_| ForwardVerifyError::MalformedField)?;

            // 2. Signature over the canonical transcript.
            let sig = Signature::from_bytes(&self.sig.0);
            let msg =
                forward_signing_input(&self.dispatch.id.0, &self.sender_relay_id, self.timestamp);
            vk.verify_strict(&msg, &sig).map_err(|_| ForwardVerifyError::BadForwardSig)?;

            // 3. Timestamp freshness — split stale-vs-future so the home relay can log them
            //    distinctly. `MAX_DHT_HELLO_SKEW_MS` is the same window used for `DhtHello` and
            //    resolver-side `RelayHello` so a relay's clock-drift behaviour is consistent across
            //    packet kinds.
            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(ForwardVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(ForwardVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }

    impl QueueFetch {
        /// Validate the user signature on a [`QueueFetch`] plus the
        /// timestamp window.
        ///
        /// The user's IPK *is* the verifying key — no external pubkey
        /// lookup needed. Steps:
        ///
        /// 1. **Pubkey shape**: `user_ipk` parses as Ed25519.
        /// 2. **Signature**: `user_sig` verifies under `user_ipk` over
        ///    [`queue_fetch_signing_input`].
        /// 3. **Timestamp window**: split stale / future per [`Forward::verify`].
        pub fn verify(&self, now_ms: u64) -> Result<(), QueueFetchVerifyError> {
            let vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| QueueFetchVerifyError::MalformedField)?;

            let sig = Signature::from_bytes(&self.user_sig.0);
            let msg = queue_fetch_signing_input(
                &self.user_ipk.0,
                &self.requester_relay_id,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig).map_err(|_| QueueFetchVerifyError::BadUserSig)?;

            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }

    impl QueueFetchAck {
        /// Validate the user signature on a [`QueueFetchAck`] plus the
        /// id-list length and timestamp window.
        ///
        /// Steps:
        /// 1. **Length bound**: `delivered_ids.len() ≤ MAX_FETCH_QUEUE_ACK_IDS`. Done first so a
        ///    malicious requester cannot ship a 100k-id ack and force the home relay to allocate a
        ///    multi-MB signing-input vector before discovering the size violation.
        /// 2. **Pubkey shape**: `user_ipk` parses as Ed25519.
        /// 3. **Signature**: `user_sig` verifies under `user_ipk` over
        ///    [`queue_fetch_ack_signing_input`] — the transcript binds `(user_ipk,
        ///    requester_relay_id, delivered_ids, timestamp)`, so a captured signature is
        ///    non-replayable against a different requester (cross-relay replay defense; see
        ///    [`QueueFetchAck`]'s doc-comment for the threat model).
        /// 4. **Timestamp window**: split stale / future.
        ///
        /// **Note**: this verifier does *not* check that
        /// `requester_relay_id` matches the connection's authenticated
        /// peer id. That check belongs in the handler (the wire-format
        /// validator has no knowledge of the carrying connection); see
        /// `relay::dht::queue_drain::handle_queue_fetch_ack_rpc` for
        /// the corresponding handler-side enforcement.
        ///
        /// Empty `delivered_ids` is legal — see the [`QueueFetchAck`]
        /// doc-comment for the rationale.
        pub fn verify(&self, now_ms: u64) -> Result<(), QueueFetchAckVerifyError> {
            // 1. Bound check first — cheap, runs before any crypto.
            if self.delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS {
                return Err(QueueFetchAckVerifyError::TooManyIds);
            }

            // 2. Pubkey shape.
            let vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| QueueFetchAckVerifyError::MalformedField)?;

            // 3. Signature.
            let sig = Signature::from_bytes(&self.user_sig.0);
            let msg = queue_fetch_ack_signing_input(
                &self.user_ipk.0,
                &self.requester_relay_id,
                &self.delivered_ids,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig).map_err(|_| QueueFetchAckVerifyError::BadUserSig)?;

            // 4. Timestamp freshness.
            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchAckVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchAckVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==: NODE DESCRIPTOR :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Descriptor returned in [`FindNodeResp`] / [`FindValueOutcome::Closer`].
/// Carries everything a requester needs to make a first contact with a
/// previously-unknown peer (id, address, full pubkey for cert-chain
/// verification on first connect).
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDescriptor {
    /// Peer's NodeId — full 32 bytes.
    pub id:     RelayId,
    /// Peer's QUIC endpoint. `serde_with::DisplayFromStr` matches the
    /// existing convention in `client_res.rs::RelayDescriptor`.
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr:   SocketAddr,
    /// Peer's full Ed25519 identity public key, so the requester can
    /// verify the cert chain on its first connect rather than chasing a
    /// side-channel.
    pub pubkey: Bytes<32>,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  RPC PAYLOADS  :==:===:===:===:||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

// --- FindNode -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindNode {
    /// Any 256-bit Key — a `NodeId`, an `IPK`, etc. We type as
    /// `Bytes<32>` deliberately (rather than `RelayId`) to encode "this
    /// is a key, not necessarily a relay id".
    pub target:    Bytes<32>,
    /// Requester's NodeId — redundant with the cert SPKI but cheap and
    /// lets the responder index its routing-table updates without
    /// re-deriving from the cert chain.
    pub requester: RelayId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindNodeResp {
    /// Up to `k = MAX_FIND_NODE_RESULTS` closest peers responder knows
    /// of. Length-bound enforced at deserialization.
    pub closer: Vec<NodeDescriptor>,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  STICKY-HOME RELAY  :==:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||
//
// The next three RPC pairs implement the wire-format contract for the
// sticky-home-relay model: the wire types, signing-input helpers, and
// per-packet `verify` methods. Sender, recipient, and home flow logic read
// these types and call these `verify` methods without re-implementing them.
//
// Skew window: every transcript here is bound to wall-clock time the same
// way as `DhtHello` (±[`MAX_DHT_HELLO_SKEW_MS`]). Reusing the constant
// keeps a relay's per-packet skew tolerance identical no matter which
// signed packet kind we're inspecting.

// --- Forward (sender_relay → home_relay) --------------------------------

/// Domain-separation tag for the sender-relay signature on a [`Forward`]
/// RPC. Distinct suffix (`-forward-v1`) so a captured `Forward` signature
/// cannot be replayed as a `DhtHello` / presence record / tombstone /
/// queue-fetch packet, and vice versa. Future on-the-wire revisions of
/// the forward transcript bump the suffix.
pub const DHT_FORWARD_SIG_DOMAIN: &[u8] = b"promtuz-dht-forward-v1";

/// Domain for an IPK-authorized push pseudonym replica. This record contains
/// no platform token; only the gateway can resolve its pseudonym to a token.
pub const DHT_PUSH_PSEUDONYM_SIG_DOMAIN: &[u8] = b"promtuz-dht-push-pseudonym-v1";
pub const DHT_LIVE_FORWARD_SIG_DOMAIN: &[u8] = b"promtuz-dht-live-forward-v1";

pub fn push_pseudonym_signing_input(
    user_ipk: &[u8; 32], pseudonym: &[u8; 32], timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(DHT_PUSH_PSEUDONYM_SIG_DOMAIN.len() + 2 + 32 + 32 + 8);
    buf.extend_from_slice(DHT_PUSH_PSEUDONYM_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(pseudonym);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// User-authorized replication of a push pseudonym to one DHT home.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPseudonymPublish {
    pub user_ipk:  Bytes<32>,
    pub pseudonym: Bytes<32>,
    pub timestamp: u64,
    pub user_sig:  Bytes<64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPseudonymPublishResp {
    pub accepted: bool,
}

/// Sender-relay → home-relay request: please deliver-or-queue this
/// dispatch on behalf of the sending relay.
///
/// **Two-layer signing model:**
/// - `dispatch.sig` is the *sender user's* signature over the dispatch payload (built at the
///   originating client, transports unchanged).
/// - `sig` is the *sender relay's* identity-key signature over a transcript that binds
///   `(dispatch_id, sender_relay_id, timestamp)` together — this gives the home relay a
///   non-repudiable record of which relay forwarded the dispatch and at what wall-clock time, so
///   per-relay rate-limit attribution and replay defence both work.
///
/// **Field declaration order is load-bearing.** The postcard wire layout
/// and [`forward_signing_input`] both visit fields in declaration order;
/// reordering silently breaks every home relay's signature check.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    /// Unmodified dispatch payload as the sender's client minted it
    /// (already signed by the user under
    /// [`crate::proto::client_rel::DISPATCH_SIG_DOMAIN`]). Carried verbatim
    /// so the home relay can deliver to the recipient client without
    /// rewriting the payload — preserving the end-to-end signature chain.
    pub dispatch:        crate::proto::client_rel::DispatchP,
    /// Issuing relay's `BLAKE3(NodeKey)` identity. The home relay uses
    /// this for per-peer rate-limit attribution and to look up the
    /// routing-table entry that holds the verifying pubkey.
    pub sender_relay_id: crate::quic::id::NodeId,
    /// Sender-relay-local Unix time in milliseconds at the moment of
    /// signing. Bound into the transcript for ±[`MAX_DHT_HELLO_SKEW_MS`]
    /// replay defence at the home relay.
    pub timestamp:       u64,
    /// Sender-relay's Ed25519 signature over [`forward_signing_input`].
    /// The home relay pulls the verifying pubkey from its routing-table
    /// entry for `sender_relay_id` (populated by `peer/1`'s `DhtHello`
    /// handshake) and runs `verify_strict`.
    pub sig:             Bytes<64>,
}

/// Build the canonical signing transcript for [`Forward`].
///
/// Layout:
/// ```text
///   DHT_FORWARD_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///     || dispatch_id (16) || sender_relay_id (32) || timestamp (BE u64)
/// ```
///
/// **Why we only sign over `dispatch.id`** rather than the whole
/// `DispatchP`: the user's own `dispatch.sig` already authenticates
/// `(to, from, id, payload)` end-to-end. The relay-layer signature only
/// needs to nail down "this relay-id forwarded this dispatch-id at this
/// time" so that the home relay can attribute rate-limit and replay
/// decisions to a specific peer. Hashing the entire payload again at the
/// relay layer is redundant and would double the signing-input size for
/// large dispatches.
///
/// Both signing (sender_relay) and verifying (home_relay) sides call
/// this helper, which makes it the byte-for-byte contract — no second
/// implementation to keep in sync.
pub fn forward_signing_input(
    dispatch_id: &[u8; 16], sender_relay_id: &crate::quic::id::NodeId, timestamp: u64,
) -> Vec<u8> {
    // domain (varies) + version (2) + id (16) + node_id (32) + ts (8)
    let mut buf = Vec::with_capacity(
        DHT_FORWARD_SIG_DOMAIN.len() + 2 + 16 + crate::quic::id::NodeId::LEN + 8,
    );
    buf.extend_from_slice(DHT_FORWARD_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(dispatch_id);
    buf.extend_from_slice(sender_relay_id.as_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Outcome a home relay reports back for a [`Forward`] RPC. Mirrors the
/// shape of [`StoreOutcome`] / [`TombstoneOutcome`] for close-reason
/// mapping consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardOutcome {
    /// Recipient was online on this home relay; we delivered locally
    /// (mirrors the existing online-recipient short-circuit in
    /// `relay/src/quic/handler/client/events/forward.rs::deliver_now`).
    Delivered,
    /// Recipient was offline; we queued the dispatch in `cf_dht_queue`.
    Stored,
    /// We are not in the recipient's k-closest by our current routing-
    /// table view — defensive return so the sender can re-dispatch
    /// without us silently dropping the message.
    NotOwner,
    /// Per-recipient queue is at [`MAX_QUEUED_PER_RECIPIENT`]. Dispatch
    /// not stored; sender should back off.
    QueueFull,
    /// Either the embedded `dispatch.sig` (user-layer) or the outer `sig`
    /// (sender-relay-layer) failed verification.
    BadSig,
    /// Per-peer rate-limit class tripped at the home relay. Sender retries
    /// after backoff.
    RateLimited,
}

/// Reply to a [`Forward`] RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardResp {
    pub outcome: ForwardOutcome,
}

/// Sender relay → recipient home relay: deliver a signed activity only when
/// its recipient is connected locally. Homes never store this packet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityForward {
    pub activity: ActivityP,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityForwardResp {
    pub delivered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceConsent {
    pub owner:        Bytes<32>,
    pub recipient:    Bytes<32>,
    pub version:      u64,
    pub issued_at_ms: u64,
    pub granted:      bool,
    pub user_sig:     Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceLease {
    pub user:          Bytes<32>,
    pub relay_id:      crate::quic::id::NodeId,
    pub version:       u64,
    pub issued_at_ms:  u64,
    pub expires_at_ms: u64,
    pub user_sig:      Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPresenceState {
    pub recipient:      Bytes<32>,
    pub who:            Bytes<32>,
    pub lease:          PresenceLease,
    pub state:          PresenceState,
    pub version:        u64,
    pub observed_at_ms: u64,
    pub relay_pubkey:   Bytes<32>,
    pub relay_sig:      Bytes<64>,
}

pub const PRESENCE_CONSENT_SIG_DOMAIN: &[u8] = b"promtuz-presence-consent-v1";
pub const PRESENCE_LEASE_SIG_DOMAIN: &[u8] = b"promtuz-presence-lease-v1";
pub const PRESENCE_STATE_SIG_DOMAIN: &[u8] = b"promtuz-presence-state-v1";
pub const PRESENCE_LEASE_MAX_MS: u64 = 10 * 60 * 1000;
pub const PRESENCE_STATE_MAX_SKEW_MS: u64 = 60_000;

pub fn presence_consent_signing_input(
    owner: &[u8; 32], recipient: &[u8; 32], version: u64, issued_at_ms: u64, granted: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(PRESENCE_CONSENT_SIG_DOMAIN.len() + 2 + 32 + 32 + 8 + 8 + 1);
    out.extend_from_slice(PRESENCE_CONSENT_SIG_DOMAIN);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(owner);
    out.extend_from_slice(recipient);
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&issued_at_ms.to_be_bytes());
    out.push(granted as u8);
    out
}

pub fn presence_lease_signing_input(
    user: &[u8; 32], relay_id: &crate::quic::id::NodeId, version: u64, issued_at_ms: u64,
    expires_at_ms: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(PRESENCE_LEASE_SIG_DOMAIN.len() + 2 + 32 + 32 + 8 * 3);
    out.extend_from_slice(PRESENCE_LEASE_SIG_DOMAIN);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(user);
    out.extend_from_slice(relay_id.as_bytes());
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&issued_at_ms.to_be_bytes());
    out.extend_from_slice(&expires_at_ms.to_be_bytes());
    out
}

pub fn presence_state_signing_input(record: &RelayPresenceState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(PRESENCE_STATE_SIG_DOMAIN);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(&record.recipient.0);
    out.extend_from_slice(&record.who.0);
    out.extend_from_slice(&record.lease.user.0);
    out.extend_from_slice(record.lease.relay_id.as_bytes());
    out.extend_from_slice(&record.lease.version.to_be_bytes());
    out.extend_from_slice(&record.lease.issued_at_ms.to_be_bytes());
    out.extend_from_slice(&record.lease.expires_at_ms.to_be_bytes());
    out.extend_from_slice(&record.lease.user_sig.0);
    out.push(match record.state {
        PresenceState::Online => 0,
        PresenceState::Idle { .. } => 1,
        PresenceState::Offline { .. } => 2,
    });
    let state_ts = match record.state {
        PresenceState::Online => 0,
        PresenceState::Idle { since } => since,
        PresenceState::Offline { last_seen } => last_seen,
    };
    out.extend_from_slice(&state_ts.to_be_bytes());
    out.extend_from_slice(&record.version.to_be_bytes());
    out.extend_from_slice(&record.observed_at_ms.to_be_bytes());
    out.extend_from_slice(&record.relay_pubkey.0);
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceReplicationResp {
    pub accepted: bool,
}

/// Recipient homes retain this user-signed assignment long enough to attempt
/// live delivery at the relay currently serving the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveForward {
    pub dispatch:        crate::proto::client_rel::DispatchP,
    pub lease:           PresenceLease,
    pub sender_relay_id: crate::quic::id::NodeId,
    pub timestamp:       u64,
    pub sig:             Bytes<64>,
}

pub fn live_forward_signing_input(
    dispatch_id: &[u8; 16], lease: &PresenceLease, sender_relay_id: &crate::quic::id::NodeId,
    timestamp: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(DHT_LIVE_FORWARD_SIG_DOMAIN.len() + 2 + 16 + 32 + 8 + 32 + 8);
    out.extend_from_slice(DHT_LIVE_FORWARD_SIG_DOMAIN);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(dispatch_id);
    out.extend_from_slice(&lease.user.0);
    out.extend_from_slice(&lease.version.to_be_bytes());
    out.extend_from_slice(lease.relay_id.as_bytes());
    out.extend_from_slice(sender_relay_id.as_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveForwardResp {
    pub delivered: bool,
}

// --- QueueFetch (recipient_relay → home_relay) --------------------------

/// Domain-separation tag for the recipient-user signature on a
/// [`QueueFetch`] RPC. Distinct suffix (`-queue-fetch-v1`) keeps a
/// captured fetch signature non-replayable across packet kinds.
pub const DHT_QUEUE_FETCH_SIG_DOMAIN: &[u8] = b"promtuz-dht-queue-fetch-v1";

/// Recipient-relay → home-relay request: please ship the queued
/// dispatches you hold for `user_ipk` so I can deliver them.
///
/// The transcript is signed by the **user's** IPK (not the requesting
/// relay's identity) so the home relay only ships queued dispatches when
/// the user has authenticated to the requester. This is the relay-to-
/// relay analogue of the user's client-side `AckDrain`: only the user
/// can authorise their own queue to drain.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetch {
    /// User whose queue to drain. Same byte shape as
    /// [`PresenceRecord::user_ipk`].
    pub user_ipk:           Bytes<32>,
    /// Requesting relay's `BLAKE3(NodeKey)` identity. Bound into the
    /// signed transcript so a captured `user_sig` cannot be redirected
    /// to a different requester (the user authorises *this* relay to
    /// drain, not any relay that holds the captured signature).
    pub requester_relay_id: crate::quic::id::NodeId,
    /// User-local Unix time in milliseconds at the moment of signing.
    /// ±[`MAX_DHT_HELLO_SKEW_MS`] replay-defence window.
    pub timestamp:          u64,
    /// User's Ed25519 signature over [`queue_fetch_signing_input`].
    /// Verified under `user_ipk` (the user's own IPK is the verifying
    /// key, no external pubkey lookup needed).
    pub user_sig:           Bytes<64>,
}

/// Build the canonical signing transcript for [`QueueFetch`].
///
/// Layout:
/// ```text
///   DHT_QUEUE_FETCH_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///     || user_ipk (32) || requester_relay_id (32) || timestamp (BE u64)
/// ```
pub fn queue_fetch_signing_input(
    user_ipk: &[u8; 32], requester_relay_id: &crate::quic::id::NodeId, timestamp: u64,
) -> Vec<u8> {
    // domain (varies) + version (2) + ipk (32) + node_id (32) + ts (8)
    let mut buf = Vec::with_capacity(
        DHT_QUEUE_FETCH_SIG_DOMAIN.len() + 2 + 32 + crate::quic::id::NodeId::LEN + 8,
    );
    buf.extend_from_slice(DHT_QUEUE_FETCH_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(requester_relay_id.as_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Maximum number of queued [`crate::proto::client_rel::DispatchP`]s a
/// single [`QueueFetchResp`] may carry. Larger drain backlogs page over
/// multiple sequential `QueueFetch` calls (see [`QueueFetchResp::exhausted`]).
///
/// Sized to match the existing `MAX_FETCH_RECORD_BATCH` cadence so a busy
/// home relay's per-RPC fan-out has consistent memory characteristics
/// across DHT and sticky-home traffic.
pub const MAX_FETCH_QUEUE_BATCH: usize = 64;

/// Reply to a [`QueueFetch`] RPC.
///
/// Carries up to [`MAX_FETCH_QUEUE_BATCH`] queued dispatches plus an
/// `exhausted` flag the requester reads to decide whether to page
/// (`exhausted = false` → "I have more, keep asking").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchResp {
    /// Dispatches drawn from `cf_dht_queue` for the user, oldest first
    /// (the on-disk key is `recipient || ts_be || dispatch_id`, so a
    /// prefix iterator naturally yields chronological order). Bounded
    /// by [`MAX_FETCH_QUEUE_BATCH`].
    pub messages:  Vec<crate::proto::client_rel::DispatchP>,
    /// `true` iff the home relay's queue for this user is empty after
    /// this batch. The requesting relay terminates the page-loop when
    /// this is `true`.
    pub exhausted: bool,
}

// --- QueueFetchAck (recipient_relay → home_relay) -----------------------

/// Domain-separation tag for the user signature on a [`QueueFetchAck`]
/// RPC. Distinct suffix (`-queue-fetch-ack-v1`) keeps a captured ack
/// signature non-replayable across packet kinds — particularly important
/// because a forged ack would force the home relay to drop queued
/// messages.
///
/// The `-v1` suffix is preserved across the transcript extension that
/// added `requester_relay_id` after `user_ipk`, because that addition is a
/// refinement of the existing protocol rather than a distinct ack-protocol
/// revision; bumping to `-v2` would conflate "transcript-shape changed"
/// with "ack semantics changed" and force a double protocol-version bump
/// on every replica simultaneously.
pub const DHT_QUEUE_FETCH_ACK_SIG_DOMAIN: &[u8] = b"promtuz-dht-queue-fetch-ack-v1";

/// Recipient-relay → home-relay request: I successfully delivered these
/// dispatch IDs to the user; please delete them from `cf_dht_queue`.
///
/// **Why the user signs (not the relay)**: a malicious relay that simply
/// signed its own ack could force every home relay to drop a user's
/// queued messages without the user ever receiving them. Routing the ack
/// through the user's IPK signature mirrors the existing client-side
/// `CRelayPacket::AckDrain` semantics — only the user authorises
/// deletion of their own queue.
///
/// **Cross-relay replay defense via `requester_relay_id`**: binding only
/// `(user_ipk, delivered_ids, timestamp)` would let a malicious relay
/// R_evil that the user authenticated to once forward the same signed ack
/// to OTHER K-closest homes (which it learned via DHT lookup) and force
/// them to delete the listed dispatch IDs even though those dispatches may
/// not have been delivered to the user. To close this, the binding mirrors
/// [`QueueFetch::requester_relay_id`]: the requester relay id is part of
/// the signed transcript, and the home additionally checks
/// `req.requester_relay_id == authenticated_peer_id` in its handler. A
/// captured ack can no longer be redirected to a different home outside
/// the user's chosen drainer.
///
/// **Empty ack is legal**: a `delivered_ids = []` ack is a no-op and the
/// home relay's verifier accepts it. The flow doesn't currently produce
/// empty acks, but the wire format permits them so future clients can
/// send a probe-only "I'm here" ack.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchAck {
    /// User whose queue to GC. Must match the `QueueFetchResp` whose
    /// dispatches we're acking.
    pub user_ipk:           Bytes<32>,
    /// Requesting relay's `BLAKE3(NodeKey)` identity. Bound into the
    /// signed transcript so a captured `user_sig` cannot be redirected
    /// to a different home (the user authorises *this* relay to drain
    /// + ack on their behalf, not any relay that gets hold of the
    /// captured signature). Mirrors [`QueueFetch::requester_relay_id`]
    /// for the same replay-defense reason; the home additionally
    /// rejects the RPC at the handler layer if `requester_relay_id`
    /// doesn't match the connection's authenticated `DhtHello` peer
    /// id.
    pub requester_relay_id: crate::quic::id::NodeId,
    /// Dispatch IDs to delete from `cf_dht_queue`. Bounded by
    /// [`MAX_FETCH_QUEUE_ACK_IDS`] (= [`MAX_FETCH_QUEUE_BATCH`]) — one
    /// ack covers exactly one fetch batch. `Vec<[u8; 16]>` because each
    /// id is the same UUIDv7 [`crate::proto::client_rel::DispatchP::id`]
    /// shape.
    pub delivered_ids:      Vec<[u8; 16]>,
    /// User-local Unix time in milliseconds at the moment of signing.
    /// ±[`MAX_DHT_HELLO_SKEW_MS`] replay-defence window.
    pub timestamp:          u64,
    /// User's Ed25519 signature over [`queue_fetch_ack_signing_input`].
    /// Verified under `user_ipk`.
    pub user_sig:           Bytes<64>,
}

/// Build the canonical signing transcript for [`QueueFetchAck`].
///
/// Layout:
/// ```text
///   DHT_QUEUE_FETCH_ACK_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///     || user_ipk (32) || requester_relay_id (32)
///     || count (BE u32) || id_0 (16) || ... || id_n (16)
///     || timestamp (BE u64)
/// ```
///
/// The 4-byte count prefix lets the verifier sanity-check the id-list
/// length without re-deserialising the wire packet, and is the same
/// shape postcard would use for `Vec<[u8;16]>` length prefixing — but
/// done explicitly here because signing-input helpers must be
/// byte-stable across protocol revisions and not piggyback on postcard's
/// internal length encoding.
///
/// `requester_relay_id` is positioned immediately after `user_ipk`,
/// mirroring [`queue_fetch_signing_input`]'s layout. This was a
/// wire-format break vs. an earlier layout (no `requester_relay_id` in the
/// transcript) but pre-1.0 the project accepts these breaks;
/// `PROTOCOL_VERSION` already advanced past that earlier release.
pub fn queue_fetch_ack_signing_input(
    user_ipk: &[u8; 32], requester_relay_id: &crate::quic::id::NodeId, delivered_ids: &[[u8; 16]],
    timestamp: u64,
) -> Vec<u8> {
    let count = delivered_ids.len() as u32;
    // domain + version (2) + ipk (32) + node_id (32) + count (4) + n*16 + ts (8)
    let mut buf = Vec::with_capacity(
        DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len()
            + 2
            + 32
            + crate::quic::id::NodeId::LEN
            + 4
            + delivered_ids.len() * 16
            + 8,
    );
    buf.extend_from_slice(DHT_QUEUE_FETCH_ACK_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(requester_relay_id.as_bytes());
    buf.extend_from_slice(&count.to_be_bytes());
    for id in delivered_ids {
        buf.extend_from_slice(id);
    }
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Maximum number of dispatch IDs a single [`QueueFetchAck`] may carry.
/// Equals [`MAX_FETCH_QUEUE_BATCH`] so one ack covers exactly one fetch
/// batch (one round-trip per drain page).
pub const MAX_FETCH_QUEUE_ACK_IDS: usize = MAX_FETCH_QUEUE_BATCH;

/// Reply to a [`QueueFetchAck`] RPC.
///
/// **Why we ship a response at all**: the per-stream dispatcher contract
/// in `relay/src/dht/handler.rs::handle_dht_request` is one-to-one
/// (`fn(DhtRequest) -> DhtResponse`) — every request must produce a
/// response variant for the bi-stream to terminate cleanly. A wire-level
/// "no response" would force a special case in the dispatcher. A trivial
/// boolean response is cheaper than redesigning the dispatcher to handle
/// fire-and-forget RPCs, and gives the requester a positive
/// "ack-was-applied" signal so transient drops can be retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchAckResp {
    /// `true` iff the home relay applied the ack (and deleted the
    /// listed ids from `cf_dht_queue`). `false` on signature mismatch,
    /// timestamp skew, or `delivered_ids` overflow — the home relay
    /// also closes with [`crate::quic::CloseReason::DhtForwardRejected`]
    /// on hard protocol violations.
    pub ok: bool,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:==:  STICKY-HOME VERIFY ERRORS  :==:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Reasons a [`Forward`] can fail outer-layer validation.
///
/// **Important**: per the wire-format contract, [`Forward::verify`] does
/// **not** validate the embedded `DispatchP` signature. That check is the
/// home relay's responsibility at delivery time (it has access to the
/// recipient's session and any user-side bookkeeping), so layering the
/// dispatch verification into the wire-validator would conflate two
/// concerns. The home relay implements the dispatch-level check inline
/// with the deliver / queue branches in `forward.rs`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ForwardVerifyError {
    /// Convenience variant exposed for symmetry with the home-relay's
    /// `forward.rs` flow even though [`Forward::verify`] never returns it
    /// (the embedded `DispatchP::sig` check happens at delivery, not
    /// here). Reserved so a unified validator can return it without
    /// shipping a wire-format change.
    #[error("forward: bad embedded dispatch signature")]
    BadDispatchSig,
    /// Outer sender-relay signature did not validate. Maps to
    /// [`crate::quic::CloseReason::DhtForwardRejected`].
    #[error("forward: bad outer sender-relay signature")]
    BadForwardSig,
    /// `now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS`.
    #[error("forward: stale timestamp (clock skew)")]
    StaleTimestamp,
    /// `timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS`.
    #[error("forward: future timestamp (clock skew)")]
    FutureTimestamp,
    /// `sender_relay_pubkey` did not parse as Ed25519. Distinct from
    /// `BadForwardSig` so handlers can attribute key-shape problems
    /// separately from sig-mismatch.
    #[error("forward: malformed sender-relay pubkey")]
    MalformedField,
}

/// Reasons a [`QueueFetch`] can fail validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueFetchVerifyError {
    /// `user_sig` did not validate under `user_ipk`. Maps to
    /// [`crate::quic::CloseReason::DhtBadSignature`].
    #[error("queue fetch: bad user signature")]
    BadUserSig,
    /// `now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS`.
    #[error("queue fetch: stale timestamp (clock skew)")]
    StaleTimestamp,
    /// `timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS`.
    #[error("queue fetch: future timestamp (clock skew)")]
    FutureTimestamp,
    /// `user_ipk` did not parse as Ed25519. Distinct from `BadUserSig`.
    #[error("queue fetch: malformed user_ipk")]
    MalformedField,
}

/// Reasons a [`QueueFetchAck`] can fail validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueFetchAckVerifyError {
    /// `user_sig` did not validate under `user_ipk`.
    #[error("queue fetch ack: bad user signature")]
    BadUserSig,
    /// `now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS`.
    #[error("queue fetch ack: stale timestamp (clock skew)")]
    StaleTimestamp,
    /// `timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS`.
    #[error("queue fetch ack: future timestamp (clock skew)")]
    FutureTimestamp,
    /// `user_ipk` did not parse as Ed25519.
    #[error("queue fetch ack: malformed user_ipk")]
    MalformedField,
    /// `delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS`. Bounded so a
    /// malicious requester cannot ship a 100k-id ack to bloat the
    /// home relay's signing-input vector.
    #[error("queue fetch ack: delivered_ids exceeds MAX_FETCH_QUEUE_ACK_IDS")]
    TooManyIds,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==:  REQUEST / RESPONSE  :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// All inbound DHT request payloads.
///
/// The acceptor side dispatches on the variant and replies with the
/// matching [`DhtResponse`] variant via the same bi-stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtRequest {
    /// Iterative-lookup hop: return the K peers closest to a target key.
    /// The only surviving read RPC — used by bootstrap's self-lookup.
    FindNode(FindNode),
    /// Sticky-home: sender-relay → home-relay deliver-or-queue. Handled
    /// in `relay/src/dht/handler.rs`.
    Forward(Forward),
    /// Ephemeral activity fan-out to recipient homes; never persisted.
    ActivityForward(ActivityForward),
    PresenceConsent(PresenceConsent),
    PresenceState(RelayPresenceState),
    PresenceLease(PresenceLease),
    LiveForward(LiveForward),
    PushPseudonymPublish(PushPseudonymPublish),
    /// Sticky-home: recipient-relay → home-relay drain request.
    QueueFetch(QueueFetch),
    /// Sticky-home: recipient-relay → home-relay post-delivery GC of
    /// dispatch ids.
    QueueFetchAck(QueueFetchAck),

    /// MLS: owner → home-relay full-batch KeyPackage publish. Adds new
    /// records to the recipient's stash; pre-existing records survive the
    /// publish (additive / anti-pinning semantics). Handled in
    /// `relay/src/dht/mls_kp.rs`.
    KeyPackagePublish(crate::proto::mls_wire::KeyPackagePublishReq),
    /// MLS: sender-relay → home-relay pop-one KeyPackage from the
    /// target's stash. Strict one-shot per fetch (`KP_PER_FETCH = 1`).
    KeyPackageFetch(crate::proto::mls_wire::KeyPackageFetchReq),
    /// MLS: owner → home-relay incremental stash top-up. Distinct from
    /// `KeyPackagePublish` only via signing-input domain (so a captured
    /// Refill sig cannot be replayed as a Publish — the two have different
    /// replacement semantics in the wider design, although both append at
    /// the relay side).
    KeyPackageRefill(crate::proto::mls_wire::KeyPackageRefillReq),

    /// MLS: sender-relay → home-relay deliver-or-queue for a Welcome
    /// envelope. The home stores it in `cf_dht_welcome` until the
    /// recipient drains via [`Self::WelcomeFetch`].
    WelcomePublish(crate::proto::mls_wire::WelcomePublishReq),
    /// MLS: recipient-relay → home-relay drain request for the
    /// recipient's queued welcomes. Authentication mirrors `QueueFetch`
    /// (user-sig + `requester_relay_id` binding).
    WelcomeFetch(crate::proto::mls_wire::WelcomeFetchReq),
    /// MLS: recipient-relay → home-relay deletion of processed welcomes.
    /// Domain-separated from `WelcomeFetch` so a captured fetch sig
    /// can't be replayed as an ack.
    WelcomeAck(crate::proto::mls_wire::WelcomeAckReq),
}

/// All outbound DHT response payloads. Mirrored 1:1 with [`DhtRequest`]
/// — the dispatcher relies on this pairing to deserialise without an
/// out-of-band request-id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtResponse {
    FindNode(FindNodeResp),
    /// Sticky-home — reply to [`DhtRequest::Forward`].
    Forward(ForwardResp),
    /// Reply to [`DhtRequest::ActivityForward`].
    ActivityForward(ActivityForwardResp),
    PresenceConsent(PresenceReplicationResp),
    PresenceState(PresenceReplicationResp),
    PresenceLease(PresenceReplicationResp),
    LiveForward(LiveForwardResp),
    PushPseudonymPublish(PushPseudonymPublishResp),
    /// Sticky-home — reply to [`DhtRequest::QueueFetch`].
    QueueFetch(QueueFetchResp),
    /// Sticky-home — reply to [`DhtRequest::QueueFetchAck`].
    /// `QueueFetchAck` itself has no semantically meaningful return
    /// payload (it's a fire-and-forget GC nudge), but the per-stream
    /// dispatcher contract in `relay/src/dht/handler.rs` requires a
    /// response variant for every request — see the
    /// [`QueueFetchAckResp`] doc-comment for the full rationale.
    QueueFetchAck(QueueFetchAckResp),

    /// MLS — reply to [`DhtRequest::KeyPackagePublish`].
    KeyPackagePublish(crate::proto::mls_wire::KeyPackagePublishResp),
    /// MLS — reply to [`DhtRequest::KeyPackageFetch`].
    /// Wraps `Found(record, remaining, static_hash) | NoStash |
    /// NotOwner | RateLimited` as a single response (mirrors
    /// `FindValueResp::result` pattern).
    KeyPackageFetch(crate::proto::mls_wire::KeyPackageFetchResp),
    /// MLS — reply to [`DhtRequest::KeyPackageRefill`].
    KeyPackageRefill(crate::proto::mls_wire::KeyPackageRefillResp),

    /// MLS — reply to [`DhtRequest::WelcomePublish`].
    WelcomePublish(crate::proto::mls_wire::WelcomePublishResp),
    /// MLS — reply to [`DhtRequest::WelcomeFetch`].
    WelcomeFetch(crate::proto::mls_wire::WelcomeFetchResp),
    /// MLS — reply to [`DhtRequest::WelcomeAck`].
    WelcomeAck(crate::proto::mls_wire::WelcomeAckResp),
}

/// Outer DHT framing wrapper. The wire grammar is open to non-RPC traffic
/// in the future (gossip, capability bits) — keeping the `Request` /
/// `Response` discriminator at the *outer* level lets new non-RPC variants
/// slot in without breaking the existing per-variant payload codecs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtPacket {
    Request(DhtRequest),
    Response(DhtResponse),
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:===:===:  TESTS  :===:===:===:===:==||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

#[cfg(all(test, feature = "crypto"))]
mod tests {

    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::PROTOCOL_VERSION;
    use crate::crypto::get_signing_key;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;
    use crate::quic::id::NodeId;

    /// Mint a fresh Ed25519 keypair via OS-RNG. Mirrors the existing
    /// `crypto::get_signing_key` pattern at `common/src/crypto/mod.rs`
    /// — `rand_core::OsRng` is the rand_core-0.6 CSPRNG that
    /// `ed25519-dalek 2.x::SigningKey::generate` expects.
    fn fresh_signing_key() -> SigningKey {
        get_signing_key()
    }

    /// Sign a fresh, internally-consistent [`DhtHello`] with `key` at
    /// `timestamp`, mirroring the dialer-side construction in
    /// `relay/src/dht/lookup.rs::send_dht_hello`.
    fn build_dht_hello(key: &SigningKey, timestamp: u64) -> DhtHello {
        let pubkey: [u8; 32] = key.verifying_key().to_bytes();
        let node_id = NodeId::new(pubkey);
        let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp);
        let sig = key.sign(&msg);
        DhtHello { node_id, pubkey: pubkey.into(), timestamp, sig: sig.to_bytes().into() }
    }

    #[test]
    fn dht_hello_round_trip() {
        // postcard encode → decode round-trip: catches any accidental
        // missing serde-derive or non-Deserialize-able field.
        let key = fresh_signing_key();
        let hello = build_dht_hello(&key, 1_700_000_000_000);
        let bytes = hello.ser().expect("postcard serialize");
        let decoded = DhtHello::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, hello);
    }

    #[test]
    fn dht_hello_signing_input_layout_is_stable() {
        // Pin the byte-layout of the transcript so a future refactor
        // that subtly reorders fields blows up here, not weeks later
        // in production "all hellos suddenly fail" mode. Mirrors
        // `user_signing_input_layout_is_stable` above.
        let pubkey = [0u8; 32];
        let mut bytes = [0u8; 32];
        bytes[0] = 0x42;
        let node_id = NodeId::from_bytes(bytes);
        let timestamp: u64 = 0xDEAD_BEEF_CAFE_F00D;

        let buf = dht_hello_signing_input(&node_id, &pubkey, timestamp);

        // Domain (20) + version (2) + node_id (32) + pubkey (32) +
        // ts (8) = 94 bytes. Anchor on the total length so a stray
        // field change is caught immediately.
        assert_eq!(buf.len(), DHT_HELLO_SIG_DOMAIN.len() + 2 + 32 + 32 + 8);

        // Spot-check the header.
        assert!(buf.starts_with(DHT_HELLO_SIG_DOMAIN));
        let off = DHT_HELLO_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &PROTOCOL_VERSION.to_be_bytes());
        let off = off + 2;
        assert_eq!(&buf[off..off + 32], node_id.as_bytes());
        let off = off + 32;
        assert_eq!(&buf[off..off + 32], &pubkey);
        let off = off + 32;
        assert_eq!(&buf[off..off + 8], &timestamp.to_be_bytes());
    }

    #[test]
    fn dht_hello_verify_accepts_freshly_signed() {
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let hello = build_dht_hello(&key, now);
        // ±0 skew → must accept.
        hello.verify(now).expect("freshly-signed hello must verify");
        // Inside the skew window → must accept.
        hello.verify(now + MAX_DHT_HELLO_SKEW_MS - 1).expect("inside skew");
        hello.verify(now - (MAX_DHT_HELLO_SKEW_MS - 1)).expect("inside skew");
    }

    #[test]
    fn dht_hello_verify_rejects_bad_pubkey_to_id_binding() {
        // Sign with `key_a` but claim `key_b`'s NodeId. Catches the
        // attacker-presenting-a-benign-pubkey-under-different-id case
        // (mirror of `presence_record_relay_id_mismatch_fails` above).
        let key_a = fresh_signing_key();
        let key_b = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let mut hello = build_dht_hello(&key_a, now);
        // Replace node_id with a *different* identity's id while keeping
        // the original (a-derived) pubkey + sig.
        let fake_id = NodeId::new(key_b.verifying_key().to_bytes());
        hello.node_id = fake_id;
        match hello.verify(now) {
            Err(DhtHelloVerifyError::IdMismatch) => {},
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn dht_hello_verify_rejects_stale_or_future_timestamp() {
        // Both directions ~2 minutes off — far beyond the 60s skew.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;

        // Stale: timestamp ~2 minutes in the past.
        let stale = build_dht_hello(&key, now - 120_000);
        match stale.verify(now) {
            Err(DhtHelloVerifyError::ClockSkew) => {},
            other => panic!("expected ClockSkew (stale), got {other:?}"),
        }

        // Future: timestamp ~2 minutes in the future.
        let future = build_dht_hello(&key, now + 120_000);
        match future.verify(now) {
            Err(DhtHelloVerifyError::ClockSkew) => {},
            other => panic!("expected ClockSkew (future), got {other:?}"),
        }
    }

    #[test]
    fn dht_hello_verify_rejects_bad_signature() {
        // Flip one bit in the signature — verify must fail.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let mut hello = build_dht_hello(&key, now);
        hello.sig.0[0] ^= 0x01;
        match hello.verify(now) {
            Err(DhtHelloVerifyError::BadSignature) => {},
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Sticky-home relay (Forward / QueueFetch / QueueFetchAck)
    // Every wire type gets a postcard round-trip, every signing-input
    // helper gets a byte-layout pin, and every `verify` impl gets a
    // happy-path test plus negative tests covering bad-sig, stale-ts,
    // future-ts, and (for ack) too-many-ids.
    // -----------------------------------------------------------------

    use crate::proto::client_rel::DispatchP;
    use crate::proto::client_rel::dispatch_sig_message;

    /// Build a fresh, internally-consistent [`DispatchP`] signed by
    /// `from_user` to `to_user`. Mirrors the production builder at
    /// `relay/src/quic/handler/client/events/forward.rs` but inlined for
    /// the test fixture so we don't drag in a `relay`-crate dep.
    fn build_dispatch(
        from_user: &SigningKey, to_ipk: &[u8; 32], id: [u8; 16], payload: &[u8],
    ) -> DispatchP {
        let from_ipk: [u8; 32] = from_user.verifying_key().to_bytes();
        let msg = dispatch_sig_message(to_ipk, &from_ipk, &id, payload);
        let sig = from_user.sign(&msg);
        DispatchP {
            to:             (*to_ipk).into(),
            from:           from_ipk.into(),
            id:             id.into(),
            payload:        payload.to_vec().into(),
            sig:            sig.to_bytes().into(),
            accepted_at_ms: 1,
        }
    }

    /// Construct a fully-signed [`Forward`] from `(sender_relay, dispatch,
    /// timestamp)`. The signing flow mirrors the production sender-side
    /// helper — keeping the test fixture in this file so any drift between
    /// fixture and production blows up on either side.
    fn build_forward(sender_relay: &SigningKey, dispatch: DispatchP, timestamp: u64) -> Forward {
        let sender_relay_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();
        let sender_relay_id = NodeId::new(sender_relay_pubkey);
        let msg = forward_signing_input(&dispatch.id.0, &sender_relay_id, timestamp);
        let sig = sender_relay.sign(&msg);
        Forward { dispatch, sender_relay_id, timestamp, sig: sig.to_bytes().into() }
    }

    fn build_queue_fetch(
        user: &SigningKey, requester_relay_id: NodeId, timestamp: u64,
    ) -> QueueFetch {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg = queue_fetch_signing_input(&user_ipk, &requester_relay_id, timestamp);
        let sig = user.sign(&msg);
        QueueFetch {
            user_ipk: user_ipk.into(),
            requester_relay_id,
            timestamp,
            user_sig: sig.to_bytes().into(),
        }
    }

    fn build_queue_fetch_ack(
        user: &SigningKey, requester_relay_id: NodeId, delivered_ids: Vec<[u8; 16]>, timestamp: u64,
    ) -> QueueFetchAck {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let msg = queue_fetch_ack_signing_input(
            &user_ipk,
            &requester_relay_id,
            &delivered_ids,
            timestamp,
        );
        let sig = user.sign(&msg);
        QueueFetchAck {
            user_ipk: user_ipk.into(),
            requester_relay_id,
            delivered_ids,
            timestamp,
            user_sig: sig.to_bytes().into(),
        }
    }

    // ------- Signing-input layout pins -------

    #[test]
    fn forward_signing_input_layout_is_stable() {
        // Pin the byte layout. Same approach as
        // `dht_hello_signing_input_layout_is_stable`: any future field
        // re-order or width change blows up here, not weeks later in
        // production "all forward signatures suddenly invalid" mode.
        let mut id_bytes = [0u8; 16];
        id_bytes[0] = 0xAB;
        let mut node_bytes = [0u8; 32];
        node_bytes[0] = 0x42;
        let node_id = NodeId::from_bytes(node_bytes);
        let timestamp: u64 = 0xDEAD_BEEF_CAFE_F00D;

        let buf = forward_signing_input(&id_bytes, &node_id, timestamp);

        // domain + version (2) + id (16) + node_id (32) + ts (8)
        assert_eq!(buf.len(), DHT_FORWARD_SIG_DOMAIN.len() + 2 + 16 + 32 + 8);
        assert!(buf.starts_with(DHT_FORWARD_SIG_DOMAIN));
        let off = DHT_FORWARD_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &PROTOCOL_VERSION.to_be_bytes());
        let off = off + 2;
        assert_eq!(&buf[off..off + 16], &id_bytes);
        let off = off + 16;
        assert_eq!(&buf[off..off + 32], node_id.as_bytes());
        let off = off + 32;
        assert_eq!(&buf[off..off + 8], &timestamp.to_be_bytes());
    }

    #[test]
    fn queue_fetch_signing_input_layout_is_stable() {
        let ipk = [0x33u8; 32];
        let mut node_bytes = [0u8; 32];
        node_bytes[0] = 0x42;
        let node_id = NodeId::from_bytes(node_bytes);
        let timestamp: u64 = 0xDEAD_BEEF_CAFE_F00D;

        let buf = queue_fetch_signing_input(&ipk, &node_id, timestamp);

        assert_eq!(buf.len(), DHT_QUEUE_FETCH_SIG_DOMAIN.len() + 2 + 32 + 32 + 8);
        assert!(buf.starts_with(DHT_QUEUE_FETCH_SIG_DOMAIN));
        let off = DHT_QUEUE_FETCH_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &PROTOCOL_VERSION.to_be_bytes());
        let off = off + 2;
        assert_eq!(&buf[off..off + 32], &ipk);
        let off = off + 32;
        assert_eq!(&buf[off..off + 32], node_id.as_bytes());
        let off = off + 32;
        assert_eq!(&buf[off..off + 8], &timestamp.to_be_bytes());
    }

    #[test]
    fn queue_fetch_ack_signing_input_layout_is_stable() {
        let ipk = [0x77u8; 32];
        let mut node_bytes = [0u8; 32];
        node_bytes[0] = 0x42;
        let node_id = NodeId::from_bytes(node_bytes);
        let ids = vec![[0xAAu8; 16], [0xBBu8; 16]];
        let timestamp: u64 = 0xDEAD_BEEF_CAFE_F00D;

        let buf = queue_fetch_ack_signing_input(&ipk, &node_id, &ids, timestamp);

        // domain + version (2) + ipk (32) + node_id (32) + count (4)
        //   + 2*16 + ts (8)
        assert_eq!(buf.len(), DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len() + 2 + 32 + 32 + 4 + 2 * 16 + 8);
        assert!(buf.starts_with(DHT_QUEUE_FETCH_ACK_SIG_DOMAIN));
        let off = DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &PROTOCOL_VERSION.to_be_bytes());
        let off = off + 2;
        assert_eq!(&buf[off..off + 32], &ipk);
        let off = off + 32;
        // requester_relay_id binds the transcript to the requesting
        // relay so a captured ack can't be redirected to a different
        // home (mirrors `queue_fetch_signing_input` layout).
        assert_eq!(&buf[off..off + 32], node_id.as_bytes());
        let off = off + 32;
        assert_eq!(&buf[off..off + 4], &(ids.len() as u32).to_be_bytes());
        let off = off + 4;
        assert_eq!(&buf[off..off + 16], &ids[0]);
        let off = off + 16;
        assert_eq!(&buf[off..off + 16], &ids[1]);
        let off = off + 16;
        assert_eq!(&buf[off..off + 8], &timestamp.to_be_bytes());
    }

    #[test]
    fn sticky_home_domain_strings_are_distinct() {
        // Four distinct domain tags — captured signature on one packet
        // kind must not be replayable as another. Mirrors the
        // implicit invariant the existing DHT domain strings already
        // hold.
        let domains = [
            DHT_FORWARD_SIG_DOMAIN,
            DHT_QUEUE_FETCH_SIG_DOMAIN,
            DHT_QUEUE_FETCH_ACK_SIG_DOMAIN,
            // Cross-check against the hello domain so a future typo
            // doesn't reuse it.
            DHT_HELLO_SIG_DOMAIN,
        ];
        for i in 0..domains.len() {
            for j in (i + 1)..domains.len() {
                assert_ne!(
                    domains[i], domains[j],
                    "domain strings must be distinct: index {i} == {j}"
                );
            }
        }
    }

    // ------- Verify happy-path tests -------

    #[test]
    fn forward_round_trip_and_verify() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_relay = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let sender_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();

        let now: u64 = 1_700_000_000_000;
        let dispatch = build_dispatch(&from_user, &to_ipk, [11u8; 16], b"hello");
        let fwd = build_forward(&sender_relay, dispatch, now);

        // Wire round-trip.
        let bytes = fwd.ser().expect("ser");
        let decoded = Forward::deser(&bytes).expect("deser");
        assert_eq!(decoded, fwd);

        // Happy-path verify: ±0 skew.
        decoded.verify(&sender_pubkey, now).expect("verify ok");
        // Inside the skew window — both sides.
        decoded
            .verify(&sender_pubkey, now + MAX_DHT_HELLO_SKEW_MS - 1)
            .expect("inside skew (forward direction)");
        decoded
            .verify(&sender_pubkey, now - (MAX_DHT_HELLO_SKEW_MS - 1))
            .expect("inside skew (backward direction)");
    }

    #[test]
    fn queue_fetch_round_trip_and_verify() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let qf = build_queue_fetch(&user, req_id, now);

        let bytes = qf.ser().expect("ser");
        let decoded = QueueFetch::deser(&bytes).expect("deser");
        assert_eq!(decoded, qf);

        decoded.verify(now).expect("verify ok");
        decoded.verify(now + MAX_DHT_HELLO_SKEW_MS - 1).expect("inside skew");
    }

    #[test]
    fn queue_fetch_ack_round_trip_and_verify() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let ack = build_queue_fetch_ack(&user, req_id, vec![[1u8; 16], [2u8; 16], [3u8; 16]], now);

        let bytes = ack.ser().expect("ser");
        let decoded = QueueFetchAck::deser(&bytes).expect("deser");
        assert_eq!(decoded, ack);

        decoded.verify(now).expect("verify ok");

        // Empty ack must also round-trip and verify cleanly — no-op
        // deletion (the wire format permits an empty id list).
        let empty = build_queue_fetch_ack(&user, req_id, Vec::new(), now);
        let empty_bytes = empty.ser().expect("ser");
        let empty_decoded = QueueFetchAck::deser(&empty_bytes).expect("deser");
        assert_eq!(empty_decoded, empty);
        empty.verify(now).expect("empty ack must verify");
    }

    // ------- Verify negative tests -------

    #[test]
    fn forward_verify_rejects_bad_outer_sig() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_relay = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let sender_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();

        let now: u64 = 1_700_000_000_000;
        let dispatch = build_dispatch(&from_user, &to_ipk, [12u8; 16], b"hi");
        let mut fwd = build_forward(&sender_relay, dispatch, now);
        // Flip a bit in the outer sender-relay signature.
        fwd.sig.0[0] ^= 0x01;
        match fwd.verify(&sender_pubkey, now) {
            Err(ForwardVerifyError::BadForwardSig) => {},
            other => panic!("expected BadForwardSig, got {other:?}"),
        }
    }

    #[test]
    fn forward_verify_rejects_stale_timestamp() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_relay = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let sender_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();

        let now: u64 = 1_700_000_000_000;
        let stale_ts = now - 120_000; // 2 minutes in the past
        let dispatch = build_dispatch(&from_user, &to_ipk, [13u8; 16], b"hi");
        let fwd = build_forward(&sender_relay, dispatch, stale_ts);
        match fwd.verify(&sender_pubkey, now) {
            Err(ForwardVerifyError::StaleTimestamp) => {},
            other => panic!("expected StaleTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn forward_verify_rejects_future_timestamp() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_relay = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let sender_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();

        let now: u64 = 1_700_000_000_000;
        let future_ts = now + 120_000; // 2 minutes in the future
        let dispatch = build_dispatch(&from_user, &to_ipk, [14u8; 16], b"hi");
        let fwd = build_forward(&sender_relay, dispatch, future_ts);
        match fwd.verify(&sender_pubkey, now) {
            Err(ForwardVerifyError::FutureTimestamp) => {},
            other => panic!("expected FutureTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn forward_verify_rejects_wrong_pubkey() {
        // Sign with `sender_a` but verify under `sender_b`'s pubkey:
        // the outer signature should fail to verify because the
        // transcript was signed by a different key.
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_a = fresh_signing_key();
        let sender_b = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let wrong_pubkey: [u8; 32] = sender_b.verifying_key().to_bytes();

        let now: u64 = 1_700_000_000_000;
        let dispatch = build_dispatch(&from_user, &to_ipk, [15u8; 16], b"hi");
        let fwd = build_forward(&sender_a, dispatch, now);
        match fwd.verify(&wrong_pubkey, now) {
            Err(ForwardVerifyError::BadForwardSig) => {},
            other => panic!("expected BadForwardSig, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_verify_rejects_bad_user_sig() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let mut qf = build_queue_fetch(&user, req_id, now);
        qf.user_sig.0[0] ^= 0x01;
        match qf.verify(now) {
            Err(QueueFetchVerifyError::BadUserSig) => {},
            other => panic!("expected BadUserSig, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_verify_rejects_stale_timestamp() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let stale_ts = now - 120_000;
        let qf = build_queue_fetch(&user, req_id, stale_ts);
        match qf.verify(now) {
            Err(QueueFetchVerifyError::StaleTimestamp) => {},
            other => panic!("expected StaleTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_ack_verify_rejects_bad_user_sig() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let mut ack = build_queue_fetch_ack(&user, req_id, vec![[1u8; 16]], now);
        ack.user_sig.0[0] ^= 0x01;
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::BadUserSig) => {},
            other => panic!("expected BadUserSig, got {other:?}"),
        }
    }

    /// Capture an ack the user signed for requester R_a, then mutate
    /// `requester_relay_id` to a different R_b (as a malicious relay
    /// would when attempting to redirect the captured ack to a different
    /// home). The signature was bound to R_a in the transcript, so
    /// verifying under the mutated R_b must fail with `BadUserSig`. This
    /// is the wire-level part of the cross-relay replay defense; the
    /// handler-side check that
    /// `requester_relay_id == authenticated_peer_id` lives in
    /// `relay::dht::queue_drain::handle_queue_fetch_ack_rpc`.
    #[test]
    fn queue_fetch_ack_verify_rejects_redirected_requester() {
        let user = fresh_signing_key();
        let req_a = fresh_signing_key();
        let req_b = fresh_signing_key();
        let req_a_id = NodeId::new(req_a.verifying_key().to_bytes());
        let req_b_id = NodeId::new(req_b.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let mut ack = build_queue_fetch_ack(&user, req_a_id, vec![[1u8; 16]], now);
        // Forward the captured ack with a different requester id (the
        // attacker's redirection attempt).
        ack.requester_relay_id = req_b_id;
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::BadUserSig) => {},
            other => panic!("expected BadUserSig, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_ack_verify_rejects_too_many_ids() {
        // Construct a (signed) ack with one too many ids — verify
        // must reject *before* hitting the signature check (the
        // signing input would also be rejected by ed25519's
        // 64-byte-sig check, but the explicit length bound is the
        // designed-in defence per the doc-comment on
        // `QueueFetchAck::verify`).
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let oversize: Vec<[u8; 16]> = (0..MAX_FETCH_QUEUE_ACK_IDS as u32 + 1)
            .map(|i| {
                let mut id = [0u8; 16];
                id[..4].copy_from_slice(&i.to_be_bytes());
                id
            })
            .collect();
        let ack = build_queue_fetch_ack(&user, req_id, oversize, now);
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::TooManyIds) => {},
            other => panic!("expected TooManyIds, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_ack_verify_rejects_stale_timestamp() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let stale_ts = now - 120_000;
        let ack = build_queue_fetch_ack(&user, req_id, vec![[1u8; 16]], stale_ts);
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::StaleTimestamp) => {},
            other => panic!("expected StaleTimestamp, got {other:?}"),
        }
    }
}
