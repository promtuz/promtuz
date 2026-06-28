//! DHT Relay-to-Relay Wire Protocol
//!
//! This module is the source of truth for the DHT relay-to-relay wire
//! protocol. It carries:
//!
//! 1. The [`PresenceRecord`] data type and its dual-signature transcripts
//!    (user_sig covering `(user_ipk, relay_id, generation)`;
//!    relay_sig covering the full record).
//! 2. The full RPC catalogue: `Ping`/`Pong`, `FindNode`/`Resp`,
//!    `FindValue`/`Resp`, `Store`/`Resp`, `Tombstone`/`Resp`,
//!    `MerkleSummary`/`Resp`, `MerkleDiff`/`Resp`, and
//!    `FetchRecord`/`Resp`.
//! 3. Length-bound constants that downstream handlers check at
//!    deserialization / construction time.
//!
//! ## Why a `DhtRequest` + `DhtResponse` split (not a single `DhtPacket`)
//!
//! We choose **separate request and response enums** plus a thin outer
//! [`DhtPacket`] wrapper because:
//!
//! - Per-RPC bi-streams: one stream carries exactly one request and one
//!   (possibly multi-frame) response, so the *direction* is implicit in
//!   the stream side. Splitting the enums means the dispatcher on each
//!   side can match exhaustively against only the variants it ever
//!   receives, instead of dynamic-checking "did the peer send a response
//!   to a question I never asked".
//! - Mirrors the exemplar in `common/src/proto/relay_res.rs` (`LifetimeP`
//!   — all packet kinds are sibling variants of one enum) but specialises
//!   it to a request/response idiom because every DHT call has exactly
//!   one of each, whereas the relay/resolver lifecycle is asymmetric.
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

use std::cmp::Ordering;
use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::proto::RelayId;
use crate::types::bytes::Bytes;

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  CONSTANTS  :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Base domain-separation tag for every DHT signed transcript.
pub const DHT_DOMAIN_PREFIX: &[u8] = b"promtuz-dht-v1";

/// Domain-separation tag for the *user-roam* signing input. U signs
/// `DHT_DOMAIN_PREFIX || "-roam-v1" || PROTOCOL_VERSION (BE u16)
/// || user_ipk || relay_id || generation (BE u64)`.
pub const DHT_USER_ROAM_SIG_DOMAIN: &[u8] = b"promtuz-dht-v1-roam-v1";

/// Domain-separation tag for the *relay-presence* counter-signing input.
/// R signs every record field except `relay_sig` itself, prefixed by
/// `DHT_DOMAIN_PREFIX || "-presence-v1" || PROTOCOL_VERSION (BE u16)`.
pub const DHT_PRESENCE_SIG_DOMAIN: &[u8] = b"promtuz-dht-v1-presence-v1";

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
/// Mirrors `HELLO_MAX_SKEW_MS` at `resolver/src/resolver/mod.rs:47` —
/// the resolver applies the same window to `RelayHello`/`RelayHeartbeat`,
/// and consistency across packet kinds keeps a relay's local clock-drift
/// behaviour identical against either receiver.
pub const MAX_DHT_HELLO_SKEW_MS: u64 = 60_000;

/// Replication factor `k`. Bounds [`FindNodeResp::closer`] and the
/// `Closer` arm of [`FindValueOutcome`].
pub const DHT_K: usize = 3;

/// Future-skew tolerance applied to [`PresenceRecord::not_before`] in
/// milliseconds. A record is rejected with
/// [`PresenceVerifyError::NotYetValid`] when `not_before > now +
/// PRESENCE_MAX_FUTURE_SKEW_MS`.
pub const PRESENCE_MAX_FUTURE_SKEW_MS: u64 = 60_000;

/// Maximum length of [`MerkleDiff::path`]. Equals tree depth: 16-bit
/// leaf address space at 4 bits per level = 4 levels.
pub const MAX_MERKLE_DIFF_PATH: usize = 4;

/// Branching factor of the per-slice radix-16 trie — also the bound on
/// [`MerkleDiffResp::Children::hashes`] length.
pub const MERKLE_FANOUT: usize = 16;

/// Bound on [`MerkleDiffResp::Leaves::entries`] per RPC. Larger result
/// sets split across multiple sequential `MerkleDiff` calls.
pub const MAX_MERKLE_DIFF_LEAVES: usize = 64;

/// Bound on [`FetchRecord::user_ipks`] and matching
/// [`FetchRecordResp::records`] per RPC.
pub const MAX_FETCH_RECORD_LEAVES: usize = 64;

/// Alias for `MAX_FETCH_RECORD_LEAVES`, exported under the name the
/// implementation prompt asked for. Same semantics, single source of
/// truth.
pub const MAX_FETCH_RECORD_BATCH: usize = MAX_FETCH_RECORD_LEAVES;

/// Bound on [`FindNodeResp::closer`] / [`FindValueOutcome::Closer`] entry
/// counts. Equal to the replication factor `k`.
pub const MAX_FIND_NODE_RESULTS: usize = DHT_K;

/// Bytes of the slice bitset in [`MerkleSummary::slices`]. 256 slices
/// over the keyspace = 32 bytes.
pub const MERKLE_SUMMARY_SLICE_BITS: usize = 256;

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
///
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
    let mut buf = Vec::with_capacity(
        DHT_HELLO_SIG_DOMAIN.len() + 2 + crate::quic::id::NodeId::LEN + 32 + 8,
    );
    buf.extend_from_slice(DHT_HELLO_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(node_id.as_bytes());
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==: PRESENCE :==:===:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// User-presence record asserting "I, relay R, host an authenticated
/// session for user U, valid until T."
///
/// Postcard-encoded. **Field declaration order is load-bearing** — both
/// the relay-side signing transcript and the postcard wire representation
/// visit fields in declaration order, so re-ordering them silently breaks
/// every replica's signature check.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceRecord {
    /// User U's Ed25519 identity public key (also the DHT key for this
    /// record). 32 bytes.
    pub user_ipk: Bytes<32>,
    /// `BLAKE3(R's NodeKey)` — the relay's full-width NodeId.
    pub relay_id: RelayId,
    /// R's full Ed25519 identity public key. Carried so verifiers do not
    /// need a side channel to recover the verification key from
    /// `relay_id` alone (a hash is non-invertible). Same justification as
    /// the `pubkey` field on `LifetimeP::RelayHello`.
    pub relay_pubkey: Bytes<32>,
    /// `not_before` in milliseconds since the Unix epoch, set by R's
    /// local clock at publish time. Bound into the relay signing
    /// transcript so a misbehaving R cannot disclaim its timestamp.
    pub not_before: u64,
    /// `not_after = not_before + PRESENCE_TTL_MS`. Replicas reject the
    /// record once the current wall-clock passes `not_after`.
    pub not_after: u64,
    /// User-controlled monotonic counter. Primary tiebreaker for
    /// multi-writer conflict resolution; also the only field that stops
    /// a replayed `user_sig` from outliving a roam.
    pub generation: u64,
    /// Bitset for future capability negotiation. v1 = `0`. Carried in the
    /// relay signing transcript so a future capability flip cannot be
    /// retroactively grafted onto an existing record.
    pub capabilities: u16,
    /// User's Ed25519 signature over
    /// [`presence_record_user_signing_input`]. Authorises R to publish on
    /// U's behalf for this `(user_ipk, relay_id, generation)` tuple.
    pub user_sig: Bytes<64>,
    /// Relay's Ed25519 signature over
    /// [`presence_record_relay_signing_input`]. Binds the *whole* record
    /// — including the timestamps — to R's identity, so a replica can
    /// attribute timestamp drift to the specific R that signed it.
    pub relay_sig: Bytes<64>,
}

/// Tombstone payload used by [`DhtRequest::Tombstone`].
///
/// A tombstone entry holds `(generation, deleted_at)` plus a relay
/// signature so a replica can attribute the deletion to the relay that
/// issued it. Tombstones are honoured by replicas for `2 ×
/// PRESENCE_TTL_MS` after `deleted_at`, then garbage-collected.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombstoneRecord {
    /// User the tombstone targets — same DHT key space as
    /// [`PresenceRecord::user_ipk`].
    pub user_ipk: Bytes<32>,
    /// Relay that issued the tombstone (= the previous record's
    /// `relay_id`).
    pub relay_id: RelayId,
    /// Issuer's full Ed25519 pubkey, for verification (mirrors
    /// [`PresenceRecord::relay_pubkey`]).
    pub relay_pubkey: Bytes<32>,
    /// Generation of the record being tombstoned. A tombstone with
    /// `generation` strictly less than the locally-held record's
    /// generation is ignored.
    pub generation: u64,
    /// Wall-clock at which R observed the disconnect, in ms since epoch.
    pub deleted_at: u64,
    /// R's Ed25519 signature over
    /// [`tombstone_signing_input`].
    pub relay_sig: Bytes<64>,
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:==:  SIGNING TRANSCRIPT HELPERS  :==:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Build the canonical user-roam signing transcript.
///
/// Layout:
/// ```text
///   DHT_DOMAIN_PREFIX || b"-roam-v1" || PROTOCOL_VERSION (BE u16)
///     || user_ipk (32) || relay_id (32) || generation (BE u64)
/// ```
///
/// Both U (signing) and the relay-side verifier call this helper, which
/// makes it the byte-for-byte contract between the two sides — there is
/// no second implementation to keep in sync.
pub fn presence_record_user_signing_input(
    user_ipk: &[u8; 32], relay_id: &RelayId, generation: u64,
) -> Vec<u8> {
    let mut buf =
        Vec::with_capacity(DHT_USER_ROAM_SIG_DOMAIN.len() + 2 + 32 + RelayId::LEN + 8);
    buf.extend_from_slice(DHT_USER_ROAM_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(relay_id.as_bytes());
    buf.extend_from_slice(&generation.to_be_bytes());
    buf
}

/// Build the canonical relay-presence countersigning transcript.
///
/// The relay signs over the wire-canonical bytes of every other field,
/// in declaration order, prefixed with
/// `DHT_DOMAIN_PREFIX || b"-presence-v1" || PROTOCOL_VERSION (BE u16)`.
/// The order below mirrors [`PresenceRecord`] field order *exactly* —
/// reorder one and the signature stops verifying.
//
// Eight args mirrors the eight signed wire fields one-to-one;
// bundling into a struct here would just add an indirection without
// changing the semantics, so we accept the arity.
#[allow(clippy::too_many_arguments)]
pub fn presence_record_relay_signing_input(
    user_ipk: &[u8; 32], relay_id: &RelayId, relay_pubkey: &[u8; 32], not_before: u64,
    not_after: u64, generation: u64, capabilities: u16, user_sig: &[u8; 64],
) -> Vec<u8> {
    // domain (varies) + version (2) + ipk (32) + relay_id (RelayId::LEN) +
    // relay_pubkey (32) + 3 × u64 (8 each) + capabilities (2) + user_sig (64)
    let mut buf = Vec::with_capacity(
        DHT_PRESENCE_SIG_DOMAIN.len() + 2 + 32 + RelayId::LEN + 32 + 8 + 8 + 8 + 2 + 64,
    );
    buf.extend_from_slice(DHT_PRESENCE_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(relay_id.as_bytes());
    buf.extend_from_slice(relay_pubkey);
    buf.extend_from_slice(&not_before.to_be_bytes());
    buf.extend_from_slice(&not_after.to_be_bytes());
    buf.extend_from_slice(&generation.to_be_bytes());
    buf.extend_from_slice(&capabilities.to_be_bytes());
    buf.extend_from_slice(user_sig);
    buf
}

/// Domain-separation tag for [`tombstone_signing_input`]. A distinct
/// suffix ensures a captured `relay_sig` over a [`PresenceRecord`] cannot
/// be replayed as a tombstone (or vice versa).
pub const DHT_TOMBSTONE_SIG_DOMAIN: &[u8] = b"promtuz-dht-v1-tombstone-v1";

/// Build the canonical tombstone signing transcript.
///
/// Layout (mirrors [`presence_record_relay_signing_input`] but without a
/// `user_sig` field):
/// ```text
///   DHT_TOMBSTONE_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
///     || user_ipk (32) || relay_id (32) || relay_pubkey (32)
///     || generation (BE u64) || deleted_at (BE u64)
/// ```
pub fn tombstone_signing_input(
    user_ipk: &[u8; 32], relay_id: &RelayId, relay_pubkey: &[u8; 32], generation: u64,
    deleted_at: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        DHT_TOMBSTONE_SIG_DOMAIN.len() + 2 + 32 + RelayId::LEN + 32 + 8 + 8,
    );
    buf.extend_from_slice(DHT_TOMBSTONE_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(relay_id.as_bytes());
    buf.extend_from_slice(relay_pubkey);
    buf.extend_from_slice(&generation.to_be_bytes());
    buf.extend_from_slice(&deleted_at.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==: VERIFICATION ERROR :==:===:===:===:||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Reasons a [`PresenceRecord`] can fail validation.
///
/// Maps onto the close-reason codes defined in
/// [`crate::quic::CloseReason`]: a `BadUserSig`/`BadRelaySig` becomes
/// `DhtBadSignature`, and `Expired`/`NotYetValid` become `DhtClockSkew`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PresenceVerifyError {
    /// `user_sig` did not validate under `user_ipk`.
    #[error("presence record: bad user signature")]
    BadUserSig,
    /// `relay_sig` did not validate under the supplied `relay_pubkey`.
    #[error("presence record: bad relay signature")]
    BadRelaySig,
    /// `now > not_after` — record's TTL has elapsed.
    #[error("presence record: expired (now > not_after)")]
    Expired,
    /// `not_before > now + PRESENCE_MAX_FUTURE_SKEW_MS` — record claims a
    /// future activation outside the accepted skew window.
    #[error("presence record: not yet valid (clock skew too large)")]
    NotYetValid,
    /// `not_after <= not_before` (degenerate window) or another
    /// structural violation noticed before signature checks.
    #[error("presence record: malformed field")]
    MalformedField,
    /// `relay_pubkey` did not parse as an Ed25519 verifying key. Treated
    /// distinctly from `BadRelaySig` so callers can attribute key-shape
    /// problems separately from signature-mismatch problems.
    #[error("presence record: malformed relay pubkey")]
    MalformedRelayPubkey,
    /// `relay_id` does not match `BLAKE3(relay_pubkey)`. Caught here so a
    /// caller cannot smuggle in a relay_pubkey that signs valid
    /// transcripts but is not the identity behind the claimed
    /// `relay_id`.
    #[error("presence record: relay_id != BLAKE3(relay_pubkey)")]
    RelayIdMismatch,
}

/// Reasons a [`DhtHello`] can fail the inbound verification at
/// `relay/src/dht/handler.rs::handle_peer_connection`.
///
/// Maps onto the [`crate::quic::CloseReason`]`::Dht*` variants 1:1:
/// - [`Self::IdMismatch`], [`Self::MalformedPubkey`], [`Self::BadSignature`]
///   → `DhtBadSignature` (or `DhtMalformedKey` for malformed pubkey
///   shape — caller's choice).
/// - [`Self::ClockSkew`] → `DhtClockSkew`.
///
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

impl PresenceRecord {
    /// Multi-writer conflict resolution.
    ///
    /// Returns [`Ordering::Greater`] when `self` is the canonical winner
    /// (replicas should keep `self` and reject `other` as `Stale`),
    /// [`Ordering::Less`] when `other` wins, [`Ordering::Equal`] only on
    /// a literal byte-identical record (statistically near-impossible in
    /// the wild, exercised in tests).
    ///
    /// Order: `generation` desc, then `not_before` desc, then `relay_id`
    /// lex desc — i.e. the higher-generation / fresher-timestamp /
    /// later-lex record wins.
    pub fn compare(&self, other: &Self) -> Ordering {
        // Higher generation wins.
        match self.generation.cmp(&other.generation) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // Same generation — fresher republish wins.
        match self.not_before.cmp(&other.not_before) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // Exact tie — deterministic lex tiebreak on the *full* relay_id
        // bytes. Lex-larger wins so the operator-visible "preferred" id
        // is also the one with the larger byte string, mirroring
        // mainline Kademlia's deterministic-but-arbitrary tie rule.
        self.relay_id.as_bytes().cmp(other.relay_id.as_bytes())
    }
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==: CRYPTO-GATED VERIFY :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

#[cfg(feature = "crypto")]
mod verify_impl {
    use ed25519_dalek::Signature;
    use ed25519_dalek::Verifier;
    use ed25519_dalek::VerifyingKey;

    use super::DhtHello;
    use super::DhtHelloVerifyError;
    use super::Forward;
    use super::ForwardVerifyError;
    use super::MAX_DHT_HELLO_SKEW_MS;
    use super::MAX_FETCH_QUEUE_ACK_IDS;
    use super::PRESENCE_MAX_FUTURE_SKEW_MS;
    use super::PresenceRecord;
    use super::PresenceVerifyError;
    use super::QueueFetch;
    use super::QueueFetchAck;
    use super::QueueFetchAckVerifyError;
    use super::QueueFetchVerifyError;
    use super::dht_hello_signing_input;
    use super::forward_signing_input;
    use super::presence_record_relay_signing_input;
    use super::presence_record_user_signing_input;
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
        /// resolver-side `verify_signed_packet` at
        /// `resolver/src/resolver/mod.rs:315-346`:
        ///
        /// 1. **id ↔ pubkey binding**: `BLAKE3(pubkey) == node_id`.
        ///    Catches an attacker presenting a benign pubkey under a
        ///    different claimed `node_id`.
        /// 2. **Pubkey shape**: `pubkey` parses as an Ed25519 verifying
        ///    key. Surfaced as `MalformedPubkey` (distinct from
        ///    `BadSignature`) so operators can distinguish key-shape
        ///    problems from sig-mismatch problems.
        /// 3. **Signature**: `sig` verifies under `pubkey` over the
        ///    canonical transcript built by
        ///    [`dht_hello_signing_input`]. Uses `verify_strict` (same
        ///    choice as `resolver/src/resolver/mod.rs:332`) for the
        ///    standard small-subgroup defence.
        /// 4. **Timestamp window**: `|now_ms − timestamp| ≤
        ///    MAX_DHT_HELLO_SKEW_MS`.
        ///
        /// `now_ms` is wall-clock in milliseconds since the Unix epoch,
        /// passed in explicitly so unit tests can pin a deterministic
        /// clock.
        pub fn verify(&self, now_ms: u64) -> Result<(), DhtHelloVerifyError> {
            // 1. id-binding to pubkey. NodeId::new = BLAKE3(pubkey) —
            //    same construction every other call site uses (cf.
            //    `verify_signed_packet` and `PresenceRecord::verify`).
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
            vk.verify_strict(&msg, &sig)
                .map_err(|_| DhtHelloVerifyError::BadSignature)?;

            // 4. Timestamp freshness (replay protection).
            let skew = now_ms.abs_diff(self.timestamp);
            if skew > MAX_DHT_HELLO_SKEW_MS {
                return Err(DhtHelloVerifyError::ClockSkew);
            }

            Ok(())
        }
    }

    impl PresenceRecord {
        /// Validate the record end-to-end:
        ///
        /// 1. Structural: `not_after > not_before` (a record with a
        ///    zero-length window is always rejected, before crypto).
        /// 2. `relay_id == BLAKE3(relay_pubkey)` — prevents an attacker
        ///    from supplying a benign `relay_pubkey` that signs valid
        ///    transcripts but is not the identity behind the claimed
        ///    `relay_id`.
        /// 3. Time window: `not_before <= now +
        ///    PRESENCE_MAX_FUTURE_SKEW_MS` and `not_after > now` —
        ///    bounds usefulness of a captured record to ~10 minutes.
        /// 4. `user_sig` verifies under the embedded `user_ipk` over
        ///    [`presence_record_user_signing_input`].
        /// 5. `relay_sig` verifies under `relay_pubkey` over
        ///    [`presence_record_relay_signing_input`].
        ///
        /// `now_ms` is wall-clock in milliseconds since the Unix epoch.
        /// Callers pass it explicitly so unit tests can pin a
        /// deterministic clock and so a network-wide clock-skew shim
        /// (future) can be inserted without touching this function.
        pub fn verify(&self, now_ms: u64) -> Result<(), PresenceVerifyError> {
            // 1. Structural sanity. Cheap, runs before any crypto.
            if self.not_after <= self.not_before {
                return Err(PresenceVerifyError::MalformedField);
            }

            // 2. id-binding to pubkey. NodeId::new = BLAKE3(pubkey) —
            // same construction every other call site uses.
            let derived_id = NodeId::new(self.relay_pubkey.as_ref());
            if derived_id != self.relay_id {
                return Err(PresenceVerifyError::RelayIdMismatch);
            }

            // 3. Time window.
            if self.not_before > now_ms.saturating_add(PRESENCE_MAX_FUTURE_SKEW_MS) {
                return Err(PresenceVerifyError::NotYetValid);
            }
            if now_ms >= self.not_after {
                return Err(PresenceVerifyError::Expired);
            }

            // 4. User signature.
            let user_vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| PresenceVerifyError::BadUserSig)?;
            let user_sig = Signature::from_bytes(&self.user_sig.0);
            let user_msg = presence_record_user_signing_input(
                &self.user_ipk.0,
                &self.relay_id,
                self.generation,
            );
            user_vk
                .verify(&user_msg, &user_sig)
                .map_err(|_| PresenceVerifyError::BadUserSig)?;

            // 5. Relay countersignature.
            let relay_vk = VerifyingKey::from_bytes(&self.relay_pubkey.0)
                .map_err(|_| PresenceVerifyError::MalformedRelayPubkey)?;
            let relay_sig = Signature::from_bytes(&self.relay_sig.0);
            let relay_msg = presence_record_relay_signing_input(
                &self.user_ipk.0,
                &self.relay_id,
                &self.relay_pubkey.0,
                self.not_before,
                self.not_after,
                self.generation,
                self.capabilities,
                &self.user_sig.0,
            );
            relay_vk
                .verify(&relay_msg, &relay_sig)
                .map_err(|_| PresenceVerifyError::BadRelaySig)?;

            Ok(())
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
        /// 1. **Pubkey shape**: caller's `sender_relay_pubkey` parses
        ///    as Ed25519. Surfaced as `MalformedField`.
        /// 2. **Signature**: `sig` verifies under
        ///    `sender_relay_pubkey` over [`forward_signing_input`].
        ///    Uses `verify_strict` for small-subgroup defence (mirrors
        ///    [`DhtHello::verify`]).
        /// 3. **Timestamp window**: `|now_ms − timestamp| ≤
        ///    MAX_DHT_HELLO_SKEW_MS`. Stale and future skew surface as
        ///    distinct `StaleTimestamp` / `FutureTimestamp` error
        ///    variants so the home relay can log them separately.
        pub fn verify(
            &self, sender_relay_pubkey: &[u8; 32], now_ms: u64,
        ) -> Result<(), ForwardVerifyError> {
            // 1. Pubkey shape.
            let vk = VerifyingKey::from_bytes(sender_relay_pubkey)
                .map_err(|_| ForwardVerifyError::MalformedField)?;

            // 2. Signature over the canonical transcript.
            let sig = Signature::from_bytes(&self.sig.0);
            let msg = forward_signing_input(
                &self.dispatch.id.0,
                &self.sender_relay_id,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig)
                .map_err(|_| ForwardVerifyError::BadForwardSig)?;

            // 3. Timestamp freshness — split stale-vs-future so the home
            //    relay can log them distinctly. `MAX_DHT_HELLO_SKEW_MS` is the
            //    same window used for `DhtHello` and resolver-side
            //    `RelayHello` so a relay's clock-drift behaviour is
            //    consistent across packet kinds.
            if now_ms > self.timestamp
                && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS
            {
                return Err(ForwardVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms
                && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS
            {
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
        /// 3. **Timestamp window**: split stale / future per
        ///    [`Forward::verify`].
        pub fn verify(&self, now_ms: u64) -> Result<(), QueueFetchVerifyError> {
            let vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| QueueFetchVerifyError::MalformedField)?;

            let sig = Signature::from_bytes(&self.user_sig.0);
            let msg = queue_fetch_signing_input(
                &self.user_ipk.0,
                &self.requester_relay_id,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig)
                .map_err(|_| QueueFetchVerifyError::BadUserSig)?;

            if now_ms > self.timestamp
                && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS
            {
                return Err(QueueFetchVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms
                && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS
            {
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
        /// 1. **Length bound**: `delivered_ids.len() ≤
        ///    MAX_FETCH_QUEUE_ACK_IDS`. Done first so a malicious
        ///    requester cannot ship a 100k-id ack and force the
        ///    home relay to allocate a multi-MB signing-input vector
        ///    before discovering the size violation.
        /// 2. **Pubkey shape**: `user_ipk` parses as Ed25519.
        /// 3. **Signature**: `user_sig` verifies under `user_ipk` over
        ///    [`queue_fetch_ack_signing_input`] — the transcript binds
        ///    `(user_ipk, requester_relay_id, delivered_ids,
        ///    timestamp)`, so a captured signature is non-replayable
        ///    against a different requester (cross-relay replay
        ///    defense; see [`QueueFetchAck`]'s doc-comment for the
        ///    threat model).
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
            vk.verify_strict(&msg, &sig)
                .map_err(|_| QueueFetchAckVerifyError::BadUserSig)?;

            // 4. Timestamp freshness.
            if now_ms > self.timestamp
                && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS
            {
                return Err(QueueFetchAckVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms
                && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS
            {
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

// --- Ping / Pong --------------------------------------------------------

/// Liveness probe / RTT sample. **Unsigned** at the application layer:
/// mTLS already binds the connection to a specific cert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub nonce:     Bytes<16>,
    pub timestamp: u64,
}

/// Reply to [`Ping`]. Echoes the nonce so the requester can correlate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub nonce:     Bytes<16>,
    pub timestamp: u64,
}

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

// --- FindValue ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindValue {
    pub user_ipk:  Bytes<32>,
    pub requester: RelayId,
}

/// Three response shapes for `FindValue`.
//
// `Found(PresenceRecord)` is ~250 B while `NotPresent` is zero-sized.
// Boxing changes the postcard wire encoding (an extra layer of
// indirection in the value path), so we keep the variants inline —
// this is a wire enum, not an internal one.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindValueOutcome {
    /// Responder is in the k owners and has the record.
    Found(PresenceRecord),
    /// Responder *is* in the k closest but has no record. Authoritative
    /// "user is offline" — terminates the iterator early.
    NotPresent,
    /// Responder is not in the k closest; here are the closest peers it
    /// knows. Length-bound `MAX_FIND_NODE_RESULTS`.
    Closer(Vec<NodeDescriptor>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindValueResp {
    pub result: FindValueOutcome,
}

// --- Store --------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    pub record: PresenceRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreOutcome {
    Stored,
    /// Responder already has a strictly-newer record.
    Stale,
    /// Responder is not in the k closest owners by its current routing
    /// view; record dropped.
    NotOwner,
    /// Either user_sig or relay_sig failed to verify.
    BadSig,
    /// Record's TTL has already elapsed at the time of the STORE.
    TtlExpired,
    /// Per-source rate limit tripped.
    RateLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreResp {
    pub outcome: StoreOutcome,
}

// --- Tombstone ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub record: TombstoneRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TombstoneOutcome {
    Stored,
    /// Local record has higher generation; tombstone ignored.
    Stale,
    /// Responder not in the k closest owners.
    NotOwner,
    /// Tombstone's relay signature failed verification.
    BadSig,
    /// Per-source rate limit tripped.
    RateLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombstoneResp {
    pub outcome: TombstoneOutcome,
}

// --- MerkleSummary ------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleSummary {
    /// Bitset of which slices the requester wants roots for. 256 slices
    /// total; encoded as a fixed 32-byte array. Wrapped in [`Bytes`] so
    /// postcard ships it as a length-elided byte string rather than a
    /// length-prefixed `Vec`.
    pub slices: Bytes<32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleSummaryResp {
    /// `(slice_id, root_hash)` pairs for each requested slice the
    /// responder also has populated. Bounded by 256 (one per possible
    /// slice).
    pub roots: Vec<(u8, Bytes<32>)>,
}

// --- MerkleDiff ---------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleDiff {
    pub slice_id: u8,
    /// Path of nibble indices from the slice root (e.g. `[3, 11, 7]` =
    /// `root.children[3].children[11].children[7]`). Bounded by
    /// [`MAX_MERKLE_DIFF_PATH`] (= tree depth).
    pub path:     Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MerkleDiffResp {
    /// Path resolved to an internal node — return its child hashes.
    /// Length is exactly [`MERKLE_FANOUT`] (= 16).
    Children { hashes: Vec<Bytes<32>> },
    /// Path resolved to a leaf — return covered (user_ipk, record-hash)
    /// pairs. Bounded by [`MAX_MERKLE_DIFF_LEAVES`].
    Leaves { entries: Vec<(Bytes<32>, Bytes<32>)> },
}

// --- FetchRecord --------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRecord {
    /// Bounded by [`MAX_FETCH_RECORD_BATCH`].
    pub user_ipks: Vec<Bytes<32>>,
}

/// Reply to a [`FetchRecord`] request.
///
/// Carries **both** live records and tombstones so anti-entropy converges
/// on deletions, not only insertions. The combined length
/// `records.len() + tombstones.len()` is bounded by
/// [`MAX_FETCH_RECORD_BATCH`]; a single IPK never appears in both lists
/// (the responder returns the tombstone if present, else the record, never
/// both — tombstones supersede live records at store time, but
/// anti-entropy still has to ship the chosen one).
///
/// **Wire-format compatibility note:** prior to this widening,
/// `FetchRecordResp` was a single `records` vec. The shape change is
/// implicit-versioned by `PROTOCOL_VERSION` (declared in
/// `common::PROTOCOL_VERSION`); peers running the older shape are gated
/// by ALPN and refuse to interop. There is no separate version flag
/// because the reply payload is *not* signed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRecordResp {
    /// Live records the responder holds for the requested IPKs.
    /// Bounded — combined with [`Self::tombstones`] — by
    /// [`MAX_FETCH_RECORD_BATCH`].
    pub records:    Vec<PresenceRecord>,
    /// Tombstones the responder holds for the requested IPKs. A
    /// tombstone supersedes any local live record (conflict rules applied
    /// at store time), so anti-entropy carrying tombstones makes deletions
    /// converge across replicas. Bounded — combined with [`Self::records`]
    /// — by [`MAX_FETCH_RECORD_BATCH`].
    pub tombstones: Vec<TombstoneRecord>,
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

/// Sender-relay → home-relay request: please deliver-or-queue this
/// dispatch on behalf of the sending relay.
///
/// **Two-layer signing model:**
/// - `dispatch.sig` is the *sender user's* signature over the dispatch
///   payload (built at the originating client, transports unchanged).
/// - `sig` is the *sender relay's* identity-key signature over a
///   transcript that binds `(dispatch_id, sender_relay_id, timestamp)`
///   together — this gives the home relay a non-repudiable record of
///   which relay forwarded the dispatch and at what wall-clock time, so
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
    pub dispatch: crate::proto::client_rel::DispatchP,
    /// Issuing relay's `BLAKE3(NodeKey)` identity. The home relay uses
    /// this for per-peer rate-limit attribution and to look up the
    /// routing-table entry that holds the verifying pubkey.
    pub sender_relay_id: crate::quic::id::NodeId,
    /// Sender-relay-local Unix time in milliseconds at the moment of
    /// signing. Bound into the transcript for ±[`MAX_DHT_HELLO_SKEW_MS`]
    /// replay defence at the home relay.
    pub timestamp: u64,
    /// Sender-relay's Ed25519 signature over [`forward_signing_input`].
    /// The home relay pulls the verifying pubkey from its routing-table
    /// entry for `sender_relay_id` (populated by `peer/1`'s `DhtHello`
    /// handshake) and runs `verify_strict`.
    pub sig: Bytes<64>,
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
    pub user_ipk: Bytes<32>,
    /// Requesting relay's `BLAKE3(NodeKey)` identity. Bound into the
    /// signed transcript so a captured `user_sig` cannot be redirected
    /// to a different requester (the user authorises *this* relay to
    /// drain, not any relay that holds the captured signature).
    pub requester_relay_id: crate::quic::id::NodeId,
    /// User-local Unix time in milliseconds at the moment of signing.
    /// ±[`MAX_DHT_HELLO_SKEW_MS`] replay-defence window.
    pub timestamp: u64,
    /// User's Ed25519 signature over [`queue_fetch_signing_input`].
    /// Verified under `user_ipk` (the user's own IPK is the verifying
    /// key, no external pubkey lookup needed).
    pub user_sig: Bytes<64>,
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
    pub messages: Vec<crate::proto::client_rel::DispatchP>,
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
    pub user_ipk: Bytes<32>,
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
    pub delivered_ids: Vec<[u8; 16]>,
    /// User-local Unix time in milliseconds at the moment of signing.
    /// ±[`MAX_DHT_HELLO_SKEW_MS`] replay-defence window.
    pub timestamp: u64,
    /// User's Ed25519 signature over [`queue_fetch_ack_signing_input`].
    /// Verified under `user_ipk`.
    pub user_sig: Bytes<64>,
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
    user_ipk: &[u8; 32], requester_relay_id: &crate::quic::id::NodeId,
    delivered_ids: &[[u8; 16]], timestamp: u64,
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
    Ping(Ping),
    FindNode(FindNode),
    FindValue(FindValue),
    Store(Store),
    Tombstone(Tombstone),
    MerkleSummary(MerkleSummary),
    MerkleDiff(MerkleDiff),
    FetchRecord(FetchRecord),
    /// Sticky-home: sender-relay → home-relay deliver-or-queue. Handled
    /// in `relay/src/dht/handler.rs`.
    Forward(Forward),
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
    Pong(Pong),
    FindNode(FindNodeResp),
    FindValue(FindValueResp),
    Store(StoreResp),
    Tombstone(TombstoneResp),
    MerkleSummary(MerkleSummaryResp),
    MerkleDiff(MerkleDiffResp),
    FetchRecord(FetchRecordResp),
    /// Sticky-home — reply to [`DhtRequest::Forward`].
    Forward(ForwardResp),
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
    use std::cmp::Ordering;

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

    /// Build a fresh, internally-consistent presence record signed by
    /// `user` (as the IPK identity) and `relay` (as the relay
    /// countersignature). All other fields are sane defaults.
    fn build_record(
        user: &SigningKey, relay: &SigningKey, generation: u64, not_before: u64,
        ttl_ms: u64,
    ) -> PresenceRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let not_after = not_before + ttl_ms;
        let capabilities: u16 = 0;

        let user_msg =
            presence_record_user_signing_input(&user_ipk, &relay_id, generation);
        let user_sig = user.sign(&user_msg);

        let relay_msg = presence_record_relay_signing_input(
            &user_ipk,
            &relay_id,
            &relay_pubkey,
            not_before,
            not_after,
            generation,
            capabilities,
            &user_sig.to_bytes(),
        );
        let relay_sig = relay.sign(&relay_msg);

        PresenceRecord {
            user_ipk: user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            not_before,
            not_after,
            generation,
            capabilities,
            user_sig: user_sig.to_bytes().into(),
            relay_sig: relay_sig.to_bytes().into(),
        }
    }

    #[test]
    fn presence_record_round_trip_and_verify() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();

        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, /* gen */ 1, /* nb */ now, /* ttl */ 600_000);

        // Serialize → deserialize. Postcard is the wire format for
        // every other proto module; staying consistent.
        let bytes = rec.ser().expect("postcard serialize");
        let decoded = PresenceRecord::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, rec);

        // verify() with a `now` inside the validity window must pass.
        decoded.verify(now + 1).expect("freshly-signed record must verify");
    }

    #[test]
    fn presence_record_tampered_user_ipk_fails_user_sig() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 600_000);

        // Flip a byte in user_ipk. The user_sig was bound to the
        // *original* ipk via the user-roam transcript, so the verify
        // step recomputes the transcript with the tampered ipk and
        // fails. (Note: verify() also re-derives relay_id from
        // relay_pubkey, so tampering with user_ipk hits user_sig
        // checking before reaching the relay_sig stage.)
        let mut tampered = rec.clone();
        tampered.user_ipk.0[0] ^= 0x01;

        match tampered.verify(now + 1) {
            Err(PresenceVerifyError::BadUserSig) => {}
            other => panic!("expected BadUserSig, got {other:?}"),
        }
    }

    #[test]
    fn presence_record_tampered_timestamp_fails_relay_sig() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let rec = build_record(&user, &relay, 1, now, 600_000);

        // Tamper with not_after (a relay-signed-only field). user_sig
        // does not cover not_after, so verification reaches the
        // relay_sig stage and fails there.
        let mut tampered = rec.clone();
        tampered.not_after += 1;

        match tampered.verify(now + 1) {
            Err(PresenceVerifyError::BadRelaySig) => {}
            other => panic!("expected BadRelaySig, got {other:?}"),
        }
    }

    #[test]
    fn presence_record_expired_window_fails() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        // ttl = 1 ms so the record is well past not_after at `now + 10`.
        let rec = build_record(&user, &relay, 1, now, 1);

        match rec.verify(now + 10) {
            Err(PresenceVerifyError::Expired) => {}
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn presence_record_far_future_fails_not_yet_valid() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        // not_before is 10 minutes in the future; verify is called with
        // a `now` well below it, exceeding the 60-second skew.
        let now: u64 = 1_700_000_000_000;
        let nb = now + 600_000;
        let rec = build_record(&user, &relay, 1, nb, 600_000);

        match rec.verify(now) {
            Err(PresenceVerifyError::NotYetValid) => {}
            other => panic!("expected NotYetValid, got {other:?}"),
        }
    }

    #[test]
    fn presence_record_relay_id_mismatch_fails() {
        let user = fresh_signing_key();
        let relay_a = fresh_signing_key();
        let relay_b = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let mut rec = build_record(&user, &relay_a, 1, now, 600_000);

        // Replace relay_pubkey with a *different* relay's pubkey while
        // keeping the original (relay_a-derived) relay_id. This
        // simulates an attacker presenting a benign-looking pubkey for
        // signature verification while smuggling a different identity.
        rec.relay_pubkey = relay_b.verifying_key().to_bytes().into();
        match rec.verify(now + 1) {
            Err(PresenceVerifyError::RelayIdMismatch) => {}
            other => panic!("expected RelayIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn presence_record_compare_orders_by_generation_desc() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let r_low = build_record(&user, &relay, /* gen */ 1, now, 600_000);
        let r_high = build_record(&user, &relay, /* gen */ 2, now, 600_000);

        // Higher generation wins.
        assert_eq!(r_high.compare(&r_low), Ordering::Greater);
        assert_eq!(r_low.compare(&r_high), Ordering::Less);
    }

    #[test]
    fn presence_record_compare_breaks_tie_by_not_before_then_relay_id() {
        let user = fresh_signing_key();
        let relay_a = fresh_signing_key();
        let relay_b = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;

        // Same generation, different not_before — fresher republish
        // wins.
        let older = build_record(&user, &relay_a, 5, now, 600_000);
        let fresher = build_record(&user, &relay_a, 5, now + 1000, 600_000);
        assert_eq!(fresher.compare(&older), Ordering::Greater);

        // Same generation, same not_before, different relay_id — lex
        // tiebreak on the full 32-byte id.
        let r1 = build_record(&user, &relay_a, 5, now, 600_000);
        let r2 = build_record(&user, &relay_b, 5, now, 600_000);
        let expected = r1.relay_id.as_bytes().cmp(r2.relay_id.as_bytes());
        assert_eq!(r1.compare(&r2), expected);
        // Comparison is total: never returns Equal between two
        // distinct relay_ids generated from independent keypairs.
        assert_ne!(expected, Ordering::Equal);
    }

    #[test]
    fn user_signing_input_layout_is_stable() {
        // Pin the byte-layout of the user transcript so a future
        // refactor that subtly reorders fields blows up here, not
        // weeks later in production-style "all signatures are
        // suddenly invalid" mode.
        let ipk = [0u8; 32];
        let mut bytes = [0u8; 32];
        bytes[0] = 0x42;
        let relay_id = RelayId::from_bytes(bytes);
        let generation: u64 = 0xDEAD_BEEF_CAFE_F00D;

        let buf = presence_record_user_signing_input(&ipk, &relay_id, generation);

        // Domain (22) + version (2) + ipk (32) + relay_id (32) + gen
        // (8) = 96 bytes. Anchor on the total length so a stray field
        // change is caught immediately.
        assert_eq!(buf.len(), DHT_USER_ROAM_SIG_DOMAIN.len() + 2 + 32 + 32 + 8);

        // Spot-check the header.
        assert!(buf.starts_with(DHT_USER_ROAM_SIG_DOMAIN));
        let off = DHT_USER_ROAM_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &PROTOCOL_VERSION.to_be_bytes());
    }

    #[test]
    fn dht_packet_round_trip_for_every_request_variant() {
        // Smoke-test postcard serialization across every RPC variant
        // — catches missing serde derives or accidental
        // non-`Deserialize`-able fields. Uses dummy data; we only
        // care about the codec round-trip.
        let pubkey = [0u8; 32];
        let id = RelayId::from_bytes([0u8; 32]);
        let dummy_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let dummy_node = NodeDescriptor {
            id,
            addr: dummy_addr,
            pubkey: pubkey.into(),
        };
        let dummy_ipk = Bytes::<32>::from([0u8; 32]);
        let dummy_record = build_record(
            &fresh_signing_key(),
            &fresh_signing_key(),
            1,
            1_700_000_000_000,
            600_000,
        );

        let cases = vec![
            DhtRequest::Ping(Ping {
                nonce:     [0u8; 16].into(),
                timestamp: 1,
            }),
            DhtRequest::FindNode(FindNode {
                target:    [1u8; 32].into(),
                requester: id,
            }),
            DhtRequest::FindValue(FindValue {
                user_ipk:  dummy_ipk,
                requester: id,
            }),
            DhtRequest::Store(Store { record: dummy_record.clone() }),
            DhtRequest::Tombstone(Tombstone {
                record: TombstoneRecord {
                    user_ipk:     dummy_ipk,
                    relay_id:     id,
                    relay_pubkey: pubkey.into(),
                    generation:   1,
                    deleted_at:   1,
                    relay_sig:    [0u8; 64].into(),
                },
            }),
            DhtRequest::MerkleSummary(MerkleSummary { slices: [0u8; 32].into() }),
            DhtRequest::MerkleDiff(MerkleDiff {
                slice_id: 0,
                path:     vec![1, 2, 3],
            }),
            DhtRequest::FetchRecord(FetchRecord {
                user_ipks: vec![dummy_ipk],
            }),
        ];

        let responses = vec![
            DhtResponse::Pong(Pong {
                nonce:     [0u8; 16].into(),
                timestamp: 1,
            }),
            DhtResponse::FindNode(FindNodeResp { closer: vec![dummy_node.clone()] }),
            DhtResponse::FindValue(FindValueResp {
                result: FindValueOutcome::Found(dummy_record.clone()),
            }),
            DhtResponse::Store(StoreResp { outcome: StoreOutcome::Stored }),
            DhtResponse::Tombstone(TombstoneResp {
                outcome: TombstoneOutcome::Stored,
            }),
            DhtResponse::MerkleSummary(MerkleSummaryResp {
                roots: vec![(0, [0u8; 32].into())],
            }),
            DhtResponse::MerkleDiff(MerkleDiffResp::Children {
                hashes: vec![[0u8; 32].into(); MERKLE_FANOUT],
            }),
            DhtResponse::FetchRecord(FetchRecordResp {
                records:    vec![dummy_record],
                tombstones: Vec::new(),
            }),
        ];

        for req in cases {
            let pkt = DhtPacket::Request(req.clone());
            let bytes = pkt.ser().expect("serialize request");
            let decoded = DhtPacket::deser(&bytes).expect("deserialize request");
            assert_eq!(decoded, pkt);
        }
        for resp in responses {
            let pkt = DhtPacket::Response(resp.clone());
            let bytes = pkt.ser().expect("serialize response");
            let decoded = DhtPacket::deser(&bytes).expect("deserialize response");
            assert_eq!(decoded, pkt);
        }
    }

    // -----------------------------------------------------------------
    // DhtHello (relay-to-relay connection-level handshake)
    // -----------------------------------------------------------------

    /// Sign a fresh, internally-consistent [`DhtHello`] with `key` at
    /// `timestamp`. Mirrors the dialer-side construction in
    /// `relay/src/dht/lookup.rs::send_dht_hello` so any drift between
    /// the production helper and the test fixture immediately blows up
    /// either side's verification.
    fn build_dht_hello(key: &SigningKey, timestamp: u64) -> DhtHello {
        let pubkey: [u8; 32] = key.verifying_key().to_bytes();
        let node_id = NodeId::new(pubkey);
        let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp);
        let sig = key.sign(&msg);
        DhtHello {
            node_id,
            pubkey: pubkey.into(),
            timestamp,
            sig: sig.to_bytes().into(),
        }
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
            Err(DhtHelloVerifyError::IdMismatch) => {}
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn dht_hello_verify_rejects_stale_timestamp() {
        // Timestamp ~2 minutes in the past — far beyond the 60s skew.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let stale_ts = now - 120_000;
        let hello = build_dht_hello(&key, stale_ts);
        match hello.verify(now) {
            Err(DhtHelloVerifyError::ClockSkew) => {}
            other => panic!("expected ClockSkew, got {other:?}"),
        }
    }

    #[test]
    fn dht_hello_verify_rejects_future_timestamp() {
        // Timestamp ~2 minutes in the future — far beyond the 60s skew.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let future_ts = now + 120_000;
        let hello = build_dht_hello(&key, future_ts);
        match hello.verify(now) {
            Err(DhtHelloVerifyError::ClockSkew) => {}
            other => panic!("expected ClockSkew, got {other:?}"),
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
            Err(DhtHelloVerifyError::BadSignature) => {}
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    #[test]
    fn dht_hello_two_relays_authenticate_each_other_synchronously() {
        // Integration-style smoke test: two `Dht`-equivalent identity
        // keys; one constructs a hello, the other verifies it. The
        // resulting authenticated NodeId matches the claimed (and
        // signed) identity. The actual QUIC round-trip is exercised
        // elsewhere.
        let dialer_key = fresh_signing_key();
        let receiver_key = fresh_signing_key();
        // dialer != receiver — receiver verifies dialer's hello.
        assert_ne!(
            dialer_key.verifying_key().to_bytes(),
            receiver_key.verifying_key().to_bytes()
        );

        let now: u64 = 1_700_000_000_000;
        let hello = build_dht_hello(&dialer_key, now);

        // Wire round-trip (would be a quinn uni-stream in production).
        let bytes = hello.ser().expect("serialize");
        let received = DhtHello::deser(&bytes).expect("deserialize");

        // Receiver verifies and stashes `received.node_id` as the
        // connection's authenticated identity.
        received.verify(now + 5).expect("hello must verify");
        let authenticated_id = received.node_id;
        assert_eq!(
            authenticated_id,
            NodeId::new(dialer_key.verifying_key().to_bytes())
        );
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
            to:      (*to_ipk).into(),
            from:    from_ipk.into(),
            id:      id.into(),
            payload: payload.to_vec().into(),
            sig:     sig.to_bytes().into(),
        }
    }

    /// Construct a fully-signed [`Forward`] from `(sender_relay, dispatch,
    /// timestamp)`. The signing flow mirrors the production sender-side
    /// helper — keeping the test fixture in this file so any drift between
    /// fixture and production blows up on either side.
    fn build_forward(
        sender_relay: &SigningKey, dispatch: DispatchP, timestamp: u64,
    ) -> Forward {
        let sender_relay_pubkey: [u8; 32] = sender_relay.verifying_key().to_bytes();
        let sender_relay_id = NodeId::new(sender_relay_pubkey);
        let msg = forward_signing_input(&dispatch.id.0, &sender_relay_id, timestamp);
        let sig = sender_relay.sign(&msg);
        Forward {
            dispatch,
            sender_relay_id,
            timestamp,
            sig: sig.to_bytes().into(),
        }
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
        user: &SigningKey, requester_relay_id: NodeId,
        delivered_ids: Vec<[u8; 16]>, timestamp: u64,
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

    // ------- Round-trip tests: postcard encode → decode → equal -------

    #[test]
    fn forward_round_trip() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let sender_relay = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();

        let dispatch = build_dispatch(&from_user, &to_ipk, [7u8; 16], b"hello");
        let fwd = build_forward(&sender_relay, dispatch, 1_700_000_000_000);

        let bytes = fwd.ser().expect("postcard serialize");
        let decoded = Forward::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, fwd);
    }

    #[test]
    fn forward_resp_round_trip() {
        // Cover every outcome variant — catches a missing serde derive
        // on a single arm (which postcard would reject at decode time).
        for outcome in [
            ForwardOutcome::Delivered,
            ForwardOutcome::Stored,
            ForwardOutcome::NotOwner,
            ForwardOutcome::QueueFull,
            ForwardOutcome::BadSig,
            ForwardOutcome::RateLimited,
        ] {
            let r = ForwardResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = ForwardResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn queue_fetch_round_trip() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let qf = build_queue_fetch(&user, req_id, 1_700_000_000_000);

        let bytes = qf.ser().expect("ser");
        let decoded = QueueFetch::deser(&bytes).expect("deser");
        assert_eq!(decoded, qf);
    }

    #[test]
    fn queue_fetch_resp_round_trip() {
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let dispatch = build_dispatch(&from_user, &to_ipk, [9u8; 16], b"x");
        for exhausted in [true, false] {
            let r = QueueFetchResp {
                messages: vec![dispatch.clone()],
                exhausted,
            };
            let bytes = r.ser().expect("ser");
            let decoded = QueueFetchResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn queue_fetch_ack_round_trip() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let ack = build_queue_fetch_ack(
            &user,
            req_id,
            vec![[1u8; 16], [2u8; 16], [3u8; 16]],
            1_700_000_000_000,
        );
        let bytes = ack.ser().expect("ser");
        let decoded = QueueFetchAck::deser(&bytes).expect("deser");
        assert_eq!(decoded, ack);

        // Empty ack must also round-trip — wire format permits it.
        let empty =
            build_queue_fetch_ack(&user, req_id, Vec::new(), 1_700_000_000_000);
        let bytes = empty.ser().expect("ser");
        let decoded = QueueFetchAck::deser(&bytes).expect("deser");
        assert_eq!(decoded, empty);
    }

    #[test]
    fn queue_fetch_ack_resp_round_trip() {
        for ok in [true, false] {
            let r = QueueFetchAckResp { ok };
            let bytes = r.ser().expect("ser");
            let decoded = QueueFetchAckResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn dht_packet_round_trip_for_sticky_home_variants() {
        // Verify the sticky-home `DhtRequest` / `DhtResponse` variants
        // slot into the outer `DhtPacket` codec without confusing the
        // discriminator. Mirrors
        // `dht_packet_round_trip_for_every_request_variant`.
        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let sender_relay = fresh_signing_key();
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());

        let dispatch = build_dispatch(&from_user, &to_ipk, [4u8; 16], b"q");
        let now: u64 = 1_700_000_000_000;
        let fwd = build_forward(&sender_relay, dispatch.clone(), now);
        let qf = build_queue_fetch(&user, req_id, now);
        let ack = build_queue_fetch_ack(&user, req_id, vec![[1u8; 16]], now);

        let requests = vec![
            DhtRequest::Forward(fwd),
            DhtRequest::QueueFetch(qf),
            DhtRequest::QueueFetchAck(ack),
        ];
        let responses = vec![
            DhtResponse::Forward(ForwardResp { outcome: ForwardOutcome::Stored }),
            DhtResponse::QueueFetch(QueueFetchResp {
                messages:  vec![dispatch.clone()],
                exhausted: true,
            }),
            DhtResponse::QueueFetchAck(QueueFetchAckResp { ok: true }),
        ];
        for r in requests {
            let pkt = DhtPacket::Request(r.clone());
            let bytes = pkt.ser().expect("ser");
            let decoded = DhtPacket::deser(&bytes).expect("deser");
            assert_eq!(decoded, pkt);
        }
        for r in responses {
            let pkt = DhtPacket::Response(r.clone());
            let bytes = pkt.ser().expect("ser");
            let decoded = DhtPacket::deser(&bytes).expect("deser");
            assert_eq!(decoded, pkt);
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
        assert_eq!(
            buf.len(),
            DHT_FORWARD_SIG_DOMAIN.len() + 2 + 16 + 32 + 8
        );
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

        assert_eq!(
            buf.len(),
            DHT_QUEUE_FETCH_SIG_DOMAIN.len() + 2 + 32 + 32 + 8
        );
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
        assert_eq!(
            buf.len(),
            DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len() + 2 + 32 + 32 + 4 + 2 * 16 + 8
        );
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
            // Cross-check against the existing DHT domain strings so a
            // future-typo doesn't reuse one of them.
            DHT_HELLO_SIG_DOMAIN,
            DHT_USER_ROAM_SIG_DOMAIN,
            DHT_PRESENCE_SIG_DOMAIN,
            DHT_TOMBSTONE_SIG_DOMAIN,
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
        decoded
            .verify(now + MAX_DHT_HELLO_SKEW_MS - 1)
            .expect("inside skew");
    }

    #[test]
    fn queue_fetch_ack_round_trip_and_verify() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let ack = build_queue_fetch_ack(
            &user,
            req_id,
            vec![[1u8; 16], [2u8; 16], [3u8; 16]],
            now,
        );

        let bytes = ack.ser().expect("ser");
        let decoded = QueueFetchAck::deser(&bytes).expect("deser");
        assert_eq!(decoded, ack);

        decoded.verify(now).expect("verify ok");

        // Empty ack must also verify cleanly — no-op deletion.
        let empty = build_queue_fetch_ack(&user, req_id, Vec::new(), now);
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
            Err(ForwardVerifyError::BadForwardSig) => {}
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
            Err(ForwardVerifyError::StaleTimestamp) => {}
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
            Err(ForwardVerifyError::FutureTimestamp) => {}
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
            Err(ForwardVerifyError::BadForwardSig) => {}
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
            Err(QueueFetchVerifyError::BadUserSig) => {}
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
            Err(QueueFetchVerifyError::StaleTimestamp) => {}
            other => panic!("expected StaleTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn queue_fetch_ack_verify_rejects_bad_user_sig() {
        let user = fresh_signing_key();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let now: u64 = 1_700_000_000_000;
        let mut ack =
            build_queue_fetch_ack(&user, req_id, vec![[1u8; 16]], now);
        ack.user_sig.0[0] ^= 0x01;
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::BadUserSig) => {}
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
        let mut ack =
            build_queue_fetch_ack(&user, req_a_id, vec![[1u8; 16]], now);
        // Forward the captured ack with a different requester id (the
        // attacker's redirection attempt).
        ack.requester_relay_id = req_b_id;
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::BadUserSig) => {}
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
        let oversize: Vec<[u8; 16]> =
            (0..MAX_FETCH_QUEUE_ACK_IDS as u32 + 1)
                .map(|i| {
                    let mut id = [0u8; 16];
                    id[..4].copy_from_slice(&i.to_be_bytes());
                    id
                })
                .collect();
        let ack = build_queue_fetch_ack(&user, req_id, oversize, now);
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::TooManyIds) => {}
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
        let ack =
            build_queue_fetch_ack(&user, req_id, vec![[1u8; 16]], stale_ts);
        match ack.verify(now) {
            Err(QueueFetchAckVerifyError::StaleTimestamp) => {}
            other => panic!("expected StaleTimestamp, got {other:?}"),
        }
    }
}
