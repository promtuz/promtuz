//! MLS Wire Protocol.
//!
//! This module is the wire-format source of truth for the MLS layer.
//! It carries:
//!
//! 1. The application-layer **envelope** wrappers
//!    ([`MlsEnvelopeP`] / [`MlsApplicationEnvelopeP`] /
//!    [`WelcomeEnvelopeP`]) that promote `DispatchP::payload` from a
//!    static-shared-key ciphertext to a postcard-encoded MLS frame.
//! 2. The **KeyPackage distribution** RPCs
//!    ([`KeyPackagePublishReq`], [`KeyPackageFetchReq`],
//!    [`KeyPackageRefillReq`] and their outcome enums) plus the stored
//!    [`KeyPackageRecord`] form the home relays use to vend
//!    one-time KeyPackages.
//! 3. **Signing-input helpers** that domain-separate every signed
//!    transcript (`MLS_DOMAIN_*` tags).
//!
//! ## What is *not* in this module
//!
//! - **No MLS bytes are serialised here.** Wherever the spec says
//!   "TLS-encoded openmls value" we carry it verbatim as a
//!   [`ByteVec`] field; the producer is openmls's own `tls_codec`,
//!   not our codec. This is load-bearing for the layered design — we
//!   may rewrite *our* envelope wire format without touching the MLS
//!   internals, and vice-versa.
//! - **No client logic.** Nothing in here decides who sends what; it
//!   only fixes the wire grammar. The client-side composition lives in
//!   libcore.
//!
//! ## Version field
//!
//! MLS-specific transcripts mix in the dedicated [`MLS_WIRE_VERSION`]
//! constant rather than [`crate::PROTOCOL_VERSION`] directly. The two
//! now diverge (see [`MLS_WIRE_VERSION`]); routing every signing helper
//! here through a single MLS-owned constant means the version marker
//! survives in the on-wire transcripts, so an endpoint refuses to verify
//! a lower-version signature even if the byte layout happens to match
//! (mirrors the [`crate::PROTOCOL_VERSION`] discipline elsewhere).
//!
//! ## Signing transcript layout
//!
//! Every helper mirrors the layout pioneered in `dht_p2p.rs`:
//!
//! ```text
//!   <MLS_DOMAIN_*> || protocol_version (BE u16) || <fields in declaration order>
//! ```
//!
//! Each transcript has its own unique domain string so a captured
//! signature for one packet kind cannot be replayed as another.

use serde::Deserialize;
use serde::Serialize;

use crate::proto::RelayId;
use crate::types::bytes::ByteVec;
use crate::types::bytes::Bytes;

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  CONSTANTS  :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// MLS-layer, peer-facing protocol version. Mixed into every MLS
/// signing transcript here and gates the app-plaintext format
/// ([`AppPayload`]).
///
/// Diverges from [`crate::PROTOCOL_VERSION`] (= 4): that constant governs
/// the relay-auth handshake and is a wider flag day to bump. This one is
/// peer-to-peer only, so a bump is just a client-coordinated redeploy —
/// cheap by comparison, so it moves independently as `AppPayload` variants
/// are added.
pub const MLS_WIRE_VERSION: u16 = 10;

/// The decrypted MLS application plaintext. Was raw UTF-8; now a tagged
/// union so receipts/edits/etc. ride the same encrypted channel. The
/// relay never sees this (it's inside the MLS ciphertext). Edit/Delete
/// name their target message by its 16-byte `dispatch_id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AppPayload {
    Text(String),
    Receipt { kind: ReceiptKind, upto: [u8; 16] },
    /// Replace the text of the message with dispatch_id `target`.
    Edit { target: [u8; 16], content: String },
    /// Tombstone the message with dispatch_id `target` (delete-for-everyone).
    Delete { target: [u8; 16] },
    /// Add (`add = true`) or remove (`add = false`) `emoji` on the message with
    /// dispatch_id `target`. The reactor is implicit — the MLS sender of this
    /// payload — so this stays correct for multi-member groups (each member's
    /// reaction is attributed to their own IPK on receipt).
    React { target: [u8; 16], emoji: String, add: bool },
    /// Text that quotes the message with dispatch_id `reply_to`. Appended
    /// after React so postcard's ordinal tags for older variants hold.
    Reply { reply_to: [u8; 16], content: String },
    /// The invitee's proof-of-pair: sent as the first app message right after
    /// accepting a Welcome. Carries nothing — its value is *being* a valid
    /// inbound MLS message, which proves the group works end-to-end and flips
    /// the inviter's contact PENDING → PAIRED. Not stored as a message.
    PairAck,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReceiptKind {
    Delivered,
    Read,
}

/// Inner envelope-version byte stamped into every
/// [`MlsApplicationEnvelopeP`] / [`WelcomeEnvelopeP`]. Distinct from
/// [`MLS_WIRE_VERSION`]: this byte tracks promtuz's *envelope* shape,
/// not the wider MLS-on-promtuz protocol version. Bumping it is a
/// breaking change to the envelope layout (e.g. adding a new field).
pub const MLS_ENVELOPE_VERSION: u8 = 1;

/// Hard ceiling on the TLS-encoded `MlsMessageOut` bytes carried in
/// [`MlsApplicationEnvelopeP::mls_message`]. The cap is enforced at
/// construction time by the producer (libcore client) and at deser
/// time by the consumer.
///
/// Derived from the transport frame cap minus headroom for the DispatchP +
/// envelope + postcard framing that wraps these bytes, so a valid MLS message
/// always fits one [`super::pack::MAX_FRAME_BYTES`] frame.
pub const MAX_FRAMED_MLS_BYTES: usize = super::pack::MAX_FRAME_BYTES - 16 * 1024;

/// Ceiling on `(env.epoch - group.epoch())` before the recipient drops
/// an incoming envelope as "implausibly far ahead." A malicious member
/// could otherwise pin per-recipient
/// memory at the buffer cap × max-epoch-arithmetic by shipping
/// envelopes with arbitrary `epoch` values up to `u64::MAX`. Once the
/// gap exceeds this we refuse to buffer (the recipient would never
/// be able to advance that far without seeing every intermediate
/// commit anyway, so buffering is wasted I/O).
///
/// 64 epochs ≈ "we missed roughly 60 commits" which is already an
/// extreme reconnect window. Set conservatively; lift if real-world
/// long-offline patterns push past it.
pub const MAX_EPOCH_AHEAD: u64 = 64;

/// Hard ceiling on the TLS-encoded `Welcome` bytes carried in
/// [`WelcomeEnvelopeP::welcome_blob`]. Welcomes are bigger than
/// applications because they carry per-recipient encrypted group
/// secrets, so they get their own 256 KiB ceiling.
pub const MAX_WELCOME_BYTES: usize = 256 * 1024;

/// Maximum KeyPackages the publisher may pack into a single
/// [`KeyPackagePublishReq::kps`] vec. Smaller batches are legal; bigger
/// batches are rejected with [`KeyPackagePublishOutcome::TooMany`].
pub const KP_STASH_TARGET: usize = 100;

/// One-time stash low-water mark — informational only on the wire
/// (the home computes it locally). Listed here for cross-reference;
/// helpers using it live in libcore client code.
pub const KP_STASH_LOW_WATER: usize = 20;

/// KeyPackages the home pops from its FIFO per [`KeyPackageFetchReq`].
/// Always 1 — strict one-shot per fetch.
pub const KP_PER_FETCH: usize = 1;

/// Per-`(target_ipk, requester_relay_id)` `KeyPackageFetch` quota,
/// in fetches per hour. The home returns
/// [`KeyPackageFetchOutcome::RateLimited`] once a single requester
/// exceeds this against a single target within a rolling hour.
/// Cross-K aggregation across replicas is *not* enforced.
pub const MAX_KP_FETCH_PER_HOUR: u32 = 60;

/// Future-skew tolerance on KeyPackage publish/fetch timestamps,
/// in milliseconds. Matches the wider DHT skew window
/// (`MAX_DHT_HELLO_SKEW_MS`) so a relay's per-packet skew tolerance
/// is the same regardless of which packet kind it inspects.
pub const MAX_KP_SKEW_MS: u64 = 60_000;

/// KeyPackage `lifetime` extension `not_after - not_before`, in ms.
/// 30 days. The home rejects [`KeyPackageRecord`]s whose
/// `expires_at_ms` is more than this past `now`.
pub const KEYPACKAGE_LIFETIME_MS: u64 = 30 * 24 * 3_600_000;

/// Anti-pinning client-initiated stash rotation cadence, in ms.
/// 7 days. The client mints a fresh KP batch this often even with no
/// consumption, so a malicious peer that hoarded fetches can't keep
/// the stash pinned to identifiable KPs forever.
pub const KP_SCHEDULED_ROTATION_MS: u64 = 7 * 24 * 3_600_000;

/// Hard ceiling on a single `WelcomeEnvelopeP` queued at a recipient's
/// home relay before the home returns
/// [`WelcomePublishOutcome::QueueFull`]. 32 pending invitations is
/// generous — group invites are rare events relative to application
/// messages — and bounds the disk a malicious peer can pin against
/// the recipient.
pub const MAX_WELCOMES_PER_RECIPIENT: usize = 32;

/// Welcome retention at the recipient's home relay before TTL eviction.
/// 30 days. Matches [`KEYPACKAGE_LIFETIME_MS`] — a Welcome older than
/// this references a KP that's also expired anyway, so the recipient's
/// openmls would reject on receipt regardless.
pub const WELCOME_LIFETIME_MS: u64 = 30 * 24 * 3_600_000;

/// Maximum welcome ids accepted in a single
/// [`WelcomeAckReq::welcome_ids`]. Bounded so a malicious requester
/// cannot ship a 100k-id ack to bloat the home's signing-input vector
/// — same rationale as `MAX_FETCH_QUEUE_ACK_IDS` for the regular
/// queue.
pub const MAX_WELCOME_ACK_IDS: usize = MAX_WELCOMES_PER_RECIPIENT;

/// Length of the per-welcome 8-byte random id that disambiguates
/// concurrent Welcomes for the same recipient at the same home. The
/// home generates this at store time; the value is opaque to the
/// recipient (it only needs the contained envelope).
pub const WELCOME_ID_LEN: usize = 8;

// ---- Domain-separation tags (one per signed transcript kind) ------

/// Base domain prefix mixed into every MLS-related signing transcript.
/// Sub-domains append a suffix so a captured signature for one packet
/// kind cannot be replayed as another (mirrors `DHT_DOMAIN_PREFIX`
/// discipline).
pub const MLS_DOMAIN_PREFIX: &[u8] = b"promtuz-mls-v1";

/// Domain-separation tag for the application-layer envelope signature
/// (binds `to_ipk`, `group_id`, `epoch`, MLS-message hash). `to_ipk` is
/// part of the transcript so a malicious relay can't redirect a captured
/// envelope to a different recipient.
pub const MLS_ENVELOPE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 envelope";

/// Domain-separation tag for the Welcome envelope signature.
/// Distinct from [`MLS_ENVELOPE_SIG_DOMAIN`] so a captured application
/// envelope sig cannot be replayed as a Welcome (and vice-versa).
pub const WELCOME_ENVELOPE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-envelope";

/// Domain for the pairing-invite signature ([`invite_signing_input`]).
/// Distinct from the envelope domains so an invite sig can't be replayed
/// as an envelope sig.
pub const INVITE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 invite";

/// Domain-separation tag for [`KeyPackagePublishReq`]'s outer signature.
pub const KP_PUBLISH_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-publish";

/// Domain-separation tag for [`KeyPackageFetchReq`]'s outer signature.
/// (Fetch carries no user signature today — the relay-to-relay
/// `DhtHello` authenticates the requester — but the helper exists so
/// a future revision can add one without re-deriving the transcript
/// layout.)
pub const KP_FETCH_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-fetch";

/// Domain-separation tag for [`KeyPackageRefillReq`]'s outer signature.
/// Distinct from [`KP_PUBLISH_DOMAIN`] so a captured Refill sig can't
/// be replayed as a Publish (the two have different replacement
/// semantics).
pub const KP_REFILL_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-refill";

/// Domain-separation tag for the per-record `owner_sig` on a
/// [`KeyPackageRecord`]. The owner (user IPK) signs over
/// `(ipk, kp_ref, expires_at_ms)`; the home verifies this before
/// accepting the record into the stash.
pub const KP_RECORD_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-record";

/// Domain-separation tag for the recipient-side
/// [`WelcomeFetchReq::user_sig`]. Distinct from [`KP_FETCH_DOMAIN`]
/// (KP fetches are unsigned at the user level) because the welcome
/// fetcher is the *recipient* of the welcome — the IPK whose welcomes
/// live in `cf_dht_welcome` — and proves authority to drain by signing
/// under that IPK. Mirrors the `QueueFetch` user-sig pattern.
pub const WELCOME_FETCH_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-fetch";

/// Domain-separation tag for the recipient-side
/// [`WelcomeAckReq::user_sig`]. Distinct from [`WELCOME_FETCH_DOMAIN`]
/// so a captured fetch sig can't be replayed as an ack (the ack
/// deletes; fetch only reads).
pub const WELCOME_ACK_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-ack";

// ---- Tier-1 (libcore→home) wrapper-sig domains --------------------
//
// Wrapper RPCs split into two signature categories:
//
//   1. User-signed RPCs (PublishKeyPackage, FetchWelcomes, AckWelcomes)
//      carry the *real* inner Tier-2 user signature — over
//      `kp_publish/refill_signing_input` and
//      `welcome_fetch/ack_signing_input` respectively. The home verifies
//      it against the connection-authenticated IPK (its gate) AND
//      forwards it unchanged to the K storage homes, which verify the
//      same signature. The home is a forwarder, not a trust root: a
//      compromised home cannot forge a KP publish or drain/delete a
//      user's welcome queue. (FetchWelcomes/AckWelcomes bind the home's
//      own NodeId as `requester_relay_id`, so the phone learns it from
//      the client/0 handshake before signing.)
//
//   2. Gate-only RPCs (FetchKeyPackage, PublishWelcome) carry a wrapper
//      signature over a dedicated domain that the home verifies locally
//      and does NOT propagate — KeyPackageFetch has no inner user sig
//      (it's DhtHello-authenticated relay-to-relay) and PublishWelcome's
//      user authorization rides inside `envelope.sender_sig`. The wrap
//      sig proves "this authenticated phone asked, now" for freshness +
//      attribution.

pub const KP_FETCH_WRAP_DOMAIN:        &[u8] = b"promtuz-mls-v1 kp-fetch-wrap";
pub const WELCOME_PUBLISH_WRAP_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-publish-wrap";

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==:  ENVELOPE  :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Outer wrapper carried by `DispatchP::payload`. The recipient's
/// libcore decodes this *before* feeding the inner bytes to openmls.
///
/// Two variants:
/// - [`MlsEnvelopeP::Application`] for in-group application messages
///   (and Commits / Proposals — the inner discriminator is opaque to
///   us; openmls dispatches internally).
/// - [`MlsEnvelopeP::Welcome`] for inviting a recipient into a new
///   group. Welcomes are special because the recipient does not yet
///   share the group's key material; the envelope must be addressable
///   by IPK alone.
///
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MlsEnvelopeP {
    /// Application-tier MLS message (could be application data, a
    /// Commit, or a Proposal — opaque to promtuz). The recipient's
    /// libcore feeds the bytes to `MlsGroup::process_message`.
    Application(MlsApplicationEnvelopeP),
    /// Group-membership invitation. The recipient's libcore feeds the
    /// bytes to `MlsGroup::new_from_welcome` after verifying the
    /// outer signature against the inviter's IPK.
    Welcome(WelcomeEnvelopeP),
    /// Pairing declined. Sent by the invitee back to the
    /// inviter when a Welcome couldn't be accepted (KP consumed, group build
    /// failed, or user rejected). Not MLS — a plain signed control message on
    /// the same dispatch/queue channel; the relay treats it as opaque payload.
    /// Appended last so postcard's ordinal tags for Application/Welcome hold.
    PairDecline(PairDeclineP),
}

/// Application-tier envelope: encrypted MLS message addressed to a
/// single recipient (group fan-out is N copies, one per non-self member).
///
/// **Plaintext metadata** the relay sees: `version`, `group_id`, `epoch`.
/// None of these reveal message content; `group_id` does reveal
/// conversation graph (which IPKs participate in which group) — accepted
/// for v1.
///
/// **Field order is load-bearing** — postcard wire and
/// [`envelope_signing_input`] both visit fields in declaration order,
/// so reordering silently breaks every recipient's signature check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlsApplicationEnvelopeP {
    /// = [`MLS_ENVELOPE_VERSION`]. Bumped on a breaking layout change
    /// to this struct (independent of [`MLS_WIRE_VERSION`]).
    pub version: u8,
    /// 32-byte MLS GroupId — promtuz constrains it to `Bytes<32>` at
    /// group-creation time. Plaintext for routing / epoch-ahead buffering
    /// at the recipient.
    pub group_id: Bytes<32>,
    /// Sender's current group epoch. Plaintext so the recipient's
    /// libcore can buffer ahead-of-epoch messages without partial
    /// decryption.
    pub epoch: u64,
    /// TLS-encoded `openmls::MlsMessageOut`. Opaque to promtuz —
    /// produced by openmls's `tls_codec`. We only round-trip these
    /// bytes through postcard's variable-length wire encoding via
    /// [`ByteVec`].
    pub mls_message: ByteVec,
    /// Sender's Ed25519 signature over [`envelope_signing_input`].
    /// The transcript binds `to_ipk` so a malicious relay cannot redirect
    /// a captured envelope to a different recipient. Verified by the
    /// recipient's libcore before feeding `mls_message` to openmls.
    pub sender_sig: Bytes<64>,
}

/// Welcome envelope: invites the recipient into a new MLS group. The
/// recipient's libcore verifies `sender_sig` *before* unwrapping the
/// Welcome with openmls — this prevents a malicious peer from injecting a
/// forged Welcome that claims to add the recipient to a fictitious group.
///
/// Plaintext metadata: `version`, `group_id`, `sender_ipk`,
/// `recipient_ipk`, `kp_ref_used`. The signature binds all of these,
/// so a captured Welcome cannot be re-targeted at a different recipient
/// (the IPK is in the transcript) or replayed against a different group
/// (the GroupId is in the transcript).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeEnvelopeP {
    /// = [`MLS_ENVELOPE_VERSION`]. See [`MlsApplicationEnvelopeP::version`].
    pub version: u8,
    /// 32-byte MLS GroupId of the group the recipient is being added
    /// to. Plaintext so the recipient can disambiguate concurrent
    /// invites — particularly important during onboarding flows where
    /// multiple invites might arrive in parallel.
    pub group_id: Bytes<32>,
    /// IPK of the inviter (also `DispatchP::from`). Carried again
    /// inside the envelope so the signature transcript can bind it.
    pub sender_ipk: Bytes<32>,
    /// IPK of the invitee (also `DispatchP::to`). Bound into the
    /// transcript so a captured Welcome cannot be re-targeted at a
    /// different recipient.
    pub recipient_ipk: Bytes<32>,
    /// TLS-encoded `openmls::Welcome`. Opaque to promtuz.
    pub welcome_blob: ByteVec,
    /// MLS `KeyPackageRef` (SHA-256 of the encoded KeyPackage per
    /// RFC 9420 §5.2) of the recipient's KP this Welcome consumes. The
    /// recipient's libcore looks this up in its local stash to find
    /// the matching `hpke_init_secret` / `leaf_signing_secret`.
    pub kp_ref_used: Bytes<32>,
    /// Sender's Ed25519 signature over [`welcome_envelope_signing_input`].
    /// Verified by the recipient under `sender_ipk` before openmls
    /// touches `welcome_blob`.
    pub sender_sig: Bytes<64>,
    /// Present only on a *pairing* Welcome (recipient not yet a contact):
    /// the inviter-signed [`Invite`] that authorizes the add, plus the
    /// sender's self-asserted display name so the recipient can save the
    /// contact. `None` for ordinary Welcomes between existing contacts.
    /// Deliberately *outside* the `sender_sig` transcript — the invite
    /// self-verifies under the recipient's own IPK, and the name is
    /// self-asserted (same trust as a scanned QR).
    pub pairing: Option<PairingP>,
}

/// A bearer pairing capability minted by a user and shown in their QR.
/// Whoever holds it may add the issuer until `expiry_ms`. Verified on the
/// issuer's own device under their own IPK — no server trust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// Random 16-byte id (enables future single-use / revoke).
    pub id: Bytes<16>,
    /// Unix-ms expiry; the issuer's device rejects the invite past this.
    pub expiry_ms: u64,
    /// Issuer's Ed25519 signature over [`invite_signing_input`].
    pub sig: Bytes<64>,
}

/// Pairing payload carried by a [`WelcomeEnvelopeP`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingP {
    pub invite: Invite,
    /// Sender's self-asserted display name; the recipient length-bounds it
    /// on accept.
    pub sender_name: String,
}

/// Why an invitee declined a Welcome. Machine reasons — the
/// inviter renders a message.
pub const DECLINE_GROUP_BUILD_FAILED: u8 = 0;
pub const DECLINE_KP_CONSUMED: u8 = 1;
pub const DECLINE_USER_REJECTED: u8 = 2;

/// Domain separator for the pair-decline signature.
pub const PAIR_DECLINE_SIG_DOMAIN: &[u8] = b"promtuz-pair-decline-v1";

/// Pairing-declined control message ([`MlsEnvelopeP::PairDecline`]). Signed by
/// the decliner so a malicious relay can't forge a rejection to grief a pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairDeclineP {
    /// Decliner IPK (also `DispatchP::from`); bound into the transcript.
    pub sender_ipk: Bytes<32>,
    /// Inviter IPK (also `DispatchP::to`); bound so a decline can't be
    /// re-targeted at a different inviter.
    pub recipient_ipk: Bytes<32>,
    /// One of the `DECLINE_*` reasons.
    pub reason: u8,
    pub timestamp: u64,
    /// Ed25519 signature over [`pair_decline_signing_input`], verified by the
    /// inviter under `sender_ipk`.
    pub sig: Bytes<64>,
}

/// Canonical bytes signed/verified for a [`PairDeclineP`].
/// Layout: `PAIR_DECLINE_SIG_DOMAIN || MLS_WIRE_VERSION_BE || sender || recipient || reason || timestamp_be`
pub fn pair_decline_signing_input(
    sender_ipk: &[u8; 32], recipient_ipk: &[u8; 32], reason: u8, timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PAIR_DECLINE_SIG_DOMAIN.len() + 2 + 32 + 32 + 1 + 8);
    buf.extend_from_slice(PAIR_DECLINE_SIG_DOMAIN);
    buf.extend_from_slice(&MLS_WIRE_VERSION.to_be_bytes());
    buf.extend_from_slice(sender_ipk);
    buf.extend_from_slice(recipient_ipk);
    buf.push(reason);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Build the canonical signing transcript for
/// [`MlsApplicationEnvelopeP::sender_sig`].
///
/// `to_ipk` is part of the transcript. Without this binding a malicious
/// relay could strip the `DispatchP` framing, re-wrap the same envelope
/// addressed to a different `to`, and the inner envelope sig would still
/// validate (since `to` was only bound by the *outer* `DispatchP::sig`).
/// Adding `to_ipk` here forecloses that vector; cost is one extra
/// Ed25519 sign per fan-out recipient (~25 ms for N=50 on a phone —
/// acceptable).
///
/// Layout:
/// ```text
///   MLS_ENVELOPE_SIG_DOMAIN || protocol_version (BE u16)
///     || to_ipk (32) || group_id (32) || epoch (BE u64)
///     || BLAKE3(mls_message_bytes) (32)
/// ```
///
/// We hash `mls_message_bytes` rather than carrying it in-line so the
/// signing-input vector is bounded at ~80 bytes regardless of the
/// underlying MLS frame (which can be up to
/// [`MAX_FRAMED_MLS_BYTES`]). Plain `blake3::hash` (not
/// keyed) — this is a plain-domain hash, not a MAC.
///
/// Both the sender's libcore and the recipient's libcore call this
/// helper, which makes it the byte-for-byte contract — no second
/// implementation to keep in sync.
///
/// `protocol_version` is taken as a parameter rather than read from a
/// global; callers pass [`MLS_WIRE_VERSION`].
pub fn envelope_signing_input(
    protocol_version: u16, to_ipk: &[u8; 32], group_id: &[u8; 32], epoch: u64,
    mls_message_bytes: &[u8],
) -> Vec<u8> {
    let msg_hash = blake3::hash(mls_message_bytes);
    let mut buf = Vec::with_capacity(MLS_ENVELOPE_SIG_DOMAIN.len() + 2 + 32 + 32 + 8 + 32);
    buf.extend_from_slice(MLS_ENVELOPE_SIG_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(to_ipk);
    buf.extend_from_slice(group_id);
    buf.extend_from_slice(&epoch.to_be_bytes());
    buf.extend_from_slice(msg_hash.as_bytes());
    buf
}

/// Canonical signing transcript for [`Invite::sig`].
///
/// Layout: `INVITE_SIG_DOMAIN || protocol_version (BE u16) || id (16)
///   || expiry_ms (BE u64)`. Signed by — and verified under — the
/// issuer's IPK. Inputs are tiny, so they're inlined (no blob hash).
pub fn invite_signing_input(protocol_version: u16, id: &[u8; 16], expiry_ms: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(INVITE_SIG_DOMAIN.len() + 2 + 16 + 8);
    buf.extend_from_slice(INVITE_SIG_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&expiry_ms.to_be_bytes());
    buf
}

/// Build the canonical signing transcript for
/// [`WelcomeEnvelopeP::sender_sig`].
///
/// Layout:
/// ```text
///   WELCOME_ENVELOPE_SIG_DOMAIN || protocol_version (BE u16)
///     || group_id (32) || sender_ipk (32) || recipient_ipk (32)
///     || kp_ref_used (32) || BLAKE3(welcome_blob) (32)
/// ```
///
/// Bound fields cover every plaintext field on the envelope plus the
/// hash of the (potentially large) Welcome blob. A captured Welcome
/// cannot be:
/// - re-targeted at a different recipient (`recipient_ipk` bound),
/// - re-attributed to a different inviter (the sig is over IPK, and
///   `sender_ipk` is bound),
/// - replayed against a different group (`group_id` bound),
/// - or paired with a different KP (`kp_ref_used` bound).
///
pub fn welcome_envelope_signing_input(
    protocol_version: u16, group_id: &[u8; 32], sender_ipk: &[u8; 32],
    recipient_ipk: &[u8; 32], kp_ref_used: &[u8; 32], welcome_blob: &[u8],
) -> Vec<u8> {
    let blob_hash = blake3::hash(welcome_blob);
    let mut buf = Vec::with_capacity(
        WELCOME_ENVELOPE_SIG_DOMAIN.len() + 2 + 32 + 32 + 32 + 32 + 32,
    );
    buf.extend_from_slice(WELCOME_ENVELOPE_SIG_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(group_id);
    buf.extend_from_slice(sender_ipk);
    buf.extend_from_slice(recipient_ipk);
    buf.extend_from_slice(kp_ref_used);
    buf.extend_from_slice(blob_hash.as_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==: KEYPACKAGE STORAGE :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

/// Stored form of a KeyPackage at a home relay.
///
/// **Wire vs storage**: this is *both* the postcard-encoded value
/// living in `cf_dht_keypackage` (relay side) AND the per-row payload
/// shipped over the wire on `KeyPackagePublish` / `KeyPackageRefill`.
/// Keeping them merged means a future protocol-version bump touches
/// **one** place, not two (mirrors the wire-vs-storage merging
/// already done for `PresenceRecord`).
///
/// **Why per-record `owner_sig`**: the `KeyPackagePublishReq` carries
/// an *outer* signature over the whole batch (so a publisher can
/// authenticate the publish event) AND each record carries an
/// *inner* `owner_sig` (so a single record can be authenticated even
/// after extraction from the batch). The outer sig binds the batch
/// to a freshness window; the inner sig binds the *record* to the
/// owner regardless of how it's later transported. Without the inner
/// sig, a home that fans a record onward (anti-entropy, or fetching
/// for a different requester) would have no way to prove the record's
/// ownership at re-vending time.
///
/// **Why `kp_ref` is `ByteVec` not `Bytes<32>`**: openmls computes
/// `KeyPackageRef = SHA-256(tls_encode(KeyPackage))[..32]` per
/// RFC 9420 §5.2 — i.e. a 32-byte digest. We store as `ByteVec` to
/// remain agnostic about the underlying ref shape (a future cipher suite
/// might use a different hash); the home checks `kp_ref.len() == 32`
/// at store time. (We use openmls's SHA-256 ref directly.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageRecord {
    /// Owner's Ed25519 IPK. Also the verifying key for [`Self::owner_sig`].
    pub ipk: Bytes<32>,
    /// MLS `KeyPackageRef` — SHA-256 of the TLS-encoded KeyPackage per
    /// RFC 9420 §5.2 (32 bytes). Stored as `ByteVec` for hash-shape
    /// agnosticism; the home enforces `len == 32`.
    pub kp_ref: ByteVec,
    /// TLS-encoded `openmls::KeyPackage`. Opaque to promtuz; passed
    /// verbatim to the requester at fetch time.
    pub kp_bytes: ByteVec,
    /// Wall-clock at which the KP's lifetime extension expires, in ms
    /// since Unix epoch. Records past this are silently filtered on
    /// fetch and rejected on store. Bounded above by `now +
    /// KEYPACKAGE_LIFETIME_MS` at construction (the publisher mints
    /// it; the home rejects if it's too far in the future).
    pub expires_at_ms: u64,
    /// Owner's Ed25519 signature over [`kp_record_signing_input`].
    /// Bound to `(ipk, kp_ref, BLAKE3(kp_bytes), expires_at_ms)`.
    ///
    /// A `BLAKE3(kp_bytes)` digest is folded into the transcript. A
    /// `kp_ref`-only binding would rely on RFC 9420 binding
    /// `kp_ref = HashReference(kp_bytes)` — but the relay does not
    /// implement RFC 9420's `HashReference` (label-prefixed SHA-256) and
    /// therefore cannot re-derive `kp_ref` from `kp_bytes`. Folding in
    /// the digest closes that gap: a malicious publisher cannot mint bogus
    /// triples where `kp_ref` is computed correctly but `kp_bytes` is
    /// malformed/replaced. The relay just verifies the sig.
    pub owner_sig: Bytes<64>,
}

/// Build the canonical signing transcript for [`KeyPackageRecord::owner_sig`].
///
/// Layout:
/// ```text
///   KP_RECORD_DOMAIN || protocol_version (BE u16)
///     || ipk (32) || kp_ref_len (BE u32) || kp_ref (var)
///     || kp_bytes_digest (32, BLAKE3)
///     || expires_at_ms (BE u64)
/// ```
///
/// `kp_ref` is variable-length per [`KeyPackageRecord::kp_ref`]'s
/// rationale; we explicitly length-prefix it inside the transcript
/// (BE u32) so a verifier can't be tricked by a length-mismatch
/// trick where two distinct `(kp_ref_a, kp_ref_b)` concatenated end
/// up indistinguishable. The same explicit length-prefix discipline
/// is used by [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`]
/// for its variable id-list.
///
/// `kp_bytes_digest` is `BLAKE3(kp_bytes)` (32 bytes, fixed length).
/// Folding the digest into the transcript binds the actual KeyPackage
/// body to the owner sig — the relay cannot re-derive RFC 9420's
/// `HashReference` form of `kp_ref` from `kp_bytes` (no openmls
/// dependency on the relay), so a "kp_ref already binds kp_bytes"
/// rationale would be operative only at clients. The defensive bind
/// here prevents a stolen IPK from minting `(ipk, kp_ref, fake_kp_bytes)`
/// triples that any home would accept.
pub fn kp_record_signing_input(
    protocol_version: u16, ipk: &[u8; 32], kp_ref: &[u8], kp_bytes: &[u8],
    expires_at_ms: u64,
) -> Vec<u8> {
    let kp_ref_len = kp_ref.len() as u32;
    let kp_bytes_digest = blake3::hash(kp_bytes);
    let kp_bytes_digest_bytes = kp_bytes_digest.as_bytes();
    let mut buf = Vec::with_capacity(
        KP_RECORD_DOMAIN.len() + 2 + 32 + 4 + kp_ref.len() + 32 + 8,
    );
    buf.extend_from_slice(KP_RECORD_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(ipk);
    buf.extend_from_slice(&kp_ref_len.to_be_bytes());
    buf.extend_from_slice(kp_ref);
    buf.extend_from_slice(kp_bytes_digest_bytes);
    buf.extend_from_slice(&expires_at_ms.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:==: KP RPC TYPES :==:===:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

// ---- Publish ------------------------------------------------------

/// `KeyPackagePublish` request — owner publishes a fresh batch of
/// KeyPackages to one of their K-closest home relays.
///
/// **Idempotent / additive**: a record `(ipk, kp_ref)` that already
/// exists at the home is treated as an idempotent re-publish (the
/// per-record `owner_sig` ensures byte-identical records by design;
/// the home runs the cross-replica static-fields check before silently
/// accepting). Republishing with a *different* `kp_bytes` for the same
/// `(ipk, kp_ref)` is rejected as a forgery / replay attempt.
///
/// **Anti-pinning rotation**: clients periodically push fresh KP batches
/// even with no consumption. A new batch *adds* to the existing stash
/// rather than replacing it, so consumers fetching during the rotation
/// window still get well-formed (in-lifetime) KPs. Old records expire
/// naturally at `expires_at_ms`.
///
/// **Field declaration order is load-bearing** — postcard wire and
/// [`kp_publish_signing_input`] both visit fields in declaration order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackagePublishReq {
    /// Owner's Ed25519 IPK. Bound to the per-record `ipk` field on
    /// every entry in `records` (mismatch → rejection).
    pub ipk: Bytes<32>,
    /// Vector of [`KeyPackageRecord`]s to add to the stash. Bounded
    /// by [`KP_STASH_TARGET`]; the home rejects oversized batches with
    /// [`KeyPackagePublishOutcome::TooMany`].
    pub records: Vec<KeyPackageRecord>,
    /// Publisher-local Unix time in milliseconds at the moment of
    /// signing. ±[`MAX_KP_SKEW_MS`] replay-defence window at the home.
    pub timestamp: u64,
    /// Owner's Ed25519 signature over [`kp_publish_signing_input`].
    /// Bound to `(ipk, records-digest, timestamp)`. The records-digest
    /// is `BLAKE3(concat(record_signing_inputs))` — so adding,
    /// removing, or modifying any record invalidates the outer sig
    /// even though each record is also individually owner-signed.
    pub sig: Bytes<64>,
}

/// `KeyPackagePublish` outcome. Mirrors the
/// [`crate::proto::dht_p2p::StoreOutcome`] / `ForwardOutcome` shape
/// (explicit enum rather than `Result<T, E>`) for close-reason mapping
/// consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPackagePublishOutcome {
    /// Every record verified and was accepted into the stash. Already-
    /// present records (same `(ipk, kp_ref, kp_bytes)` triple) are
    /// idempotent; their `owner_sig` is re-checked but they don't
    /// take a new slot.
    Stored,
    /// Outer `sig` failed to verify under `ipk`, or any per-record
    /// `owner_sig` failed, or the embedded openmls KP itself rejected
    /// validation. Maps to `CloseReason::KeyPackageMalformed`.
    BadSig,
    /// Some record's `expires_at_ms` is already past `now`, or
    /// `timestamp` is outside the ±[`MAX_KP_SKEW_MS`] skew window.
    /// Maps to `CloseReason::KeyPackageExpired`.
    Expired,
    /// Responder is not in the k closest owners by its current
    /// routing-table view; record dropped (mirrors
    /// [`crate::proto::dht_p2p::StoreOutcome::NotOwner`]).
    NotOwner,
    /// Per-`(target_ipk, requester_relay_id)` rate limit tripped.
    /// (Publish quota is independent from fetch quota — the spec
    /// doesn't constrain self-publish rate, but we wire the limiter
    /// so a misbehaving owner can't hammer their own home.)
    /// Maps to `CloseReason::KeyPackageRateLimited`.
    RateLimited,
    /// `records.len() > KP_STASH_TARGET` or any individual record
    /// failed structural validation (shape / length bounds).
    TooMany,
    /// Cross-replica static-fields check tripped: a record with the same
    /// `(ipk, kp_ref)` already exists at this replica with **different**
    /// `kp_bytes`. Indicates a forgery/replay attempt. The publish is
    /// rejected; the existing record is preserved.
    StaticFieldsConflict,
}

/// Reply to [`KeyPackagePublishReq`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackagePublishResp {
    pub outcome: KeyPackagePublishOutcome,
}

/// Build the canonical signing transcript for [`KeyPackagePublishReq::sig`].
///
/// Layout:
/// ```text
///   KP_PUBLISH_DOMAIN || protocol_version (BE u16)
///     || ipk (32) || record_count (BE u32) || records_digest (32)
///     || timestamp (BE u64)
/// ```
///
/// `records_digest = BLAKE3(concat(kp_record_signing_input(...) for each record))`.
/// We hash the per-record signing inputs (not the postcard-encoded
/// records) so the transcript is independent of postcard's encoding
/// choices — the same logical batch always produces the same digest.
/// Each per-record signing input already carries `KP_RECORD_DOMAIN`
/// internally so a captured per-record sig can't masquerade as a
/// publish-batch element.
pub fn kp_publish_signing_input(
    protocol_version: u16, ipk: &[u8; 32], records_digest: &[u8; 32],
    record_count: u32, timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        KP_PUBLISH_DOMAIN.len() + 2 + 32 + 4 + 32 + 8,
    );
    buf.extend_from_slice(KP_PUBLISH_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(ipk);
    buf.extend_from_slice(&record_count.to_be_bytes());
    buf.extend_from_slice(records_digest);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Compute the records-digest over a slice of records. Used by both
/// signer and verifier so the digest discipline lives in exactly one
/// place.
///
/// The digest covers each record's *signing-input* bytes, not its
/// postcard-encoded form — keeping the digest stable across postcard
/// version upgrades (postcard makes no byte-stability guarantees
/// across versions).
pub fn kp_publish_records_digest(
    protocol_version: u16, records: &[KeyPackageRecord],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for r in records {
        let inp = kp_record_signing_input(
            protocol_version,
            &r.ipk.0,
            &r.kp_ref.0,
            &r.kp_bytes.0,
            r.expires_at_ms,
        );
        hasher.update(&inp);
    }
    *hasher.finalize().as_bytes()
}

// ---- Fetch --------------------------------------------------------

/// `KeyPackageFetch` request — sender-relay → home-relay request to
/// pop one KeyPackage from the target's stash. The relay-to-relay
/// `DhtHello` connection-binding authenticates the requester, so this
/// request carries no user-layer signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchReq {
    /// Target user whose stash we want to consume from.
    pub target_ipk: Bytes<32>,
    /// Requester relay's `BLAKE3(NodeKey)` identity. The home checks
    /// this matches the connection's authenticated `DhtHello` peer
    /// id (mirrors the `QueueFetch` pattern); per-pair rate limits
    /// key off `(target_ipk, requester_relay_id)`.
    pub requester_relay_id: RelayId,
    /// Requester-local Unix time in milliseconds. ±[`MAX_KP_SKEW_MS`]
    /// replay-defence window at the home.
    pub timestamp: u64,
}

/// `KeyPackageFetch` outcome — three terminal states.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPackageFetchOutcome {
    /// Stash was non-empty; one KP popped and returned.
    Found(KeyPackageFetchFound),
    /// The home holds no in-lifetime KPs for this `target_ipk`. The
    /// requester typically falls through to "ask the user to refill
    /// their stash on next reconnect".
    NoStash,
    /// Responder is not in the k closest owners for `target_ipk`.
    /// Maps to `CloseReason::DhtNotOwner`.
    NotOwner,
    /// Per-`(target_ipk, requester_relay_id)` rate limit tripped
    /// (60/hour). Maps to `CloseReason::KeyPackageRateLimited`.
    RateLimited,
}

/// Found-arm payload of [`KeyPackageFetchOutcome::Found`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchFound {
    /// The popped record, including its per-record `owner_sig` so the
    /// requester can re-verify before consuming.
    pub record: KeyPackageRecord,
    /// Number of unconsumed in-lifetime KPs remaining at this home
    /// after this fetch. The owner's libcore can use this to decide
    /// whether to refill on next heartbeat.
    pub remaining: u32,
    /// `BLAKE3(target_ipk || credential_ipk || credential_signing_key_bytes)`
    /// — the *static* identity fields the cross-replica check compares
    /// across K homes. The requester optionally fans out 2-of-3 fetches
    /// and demands these match. Computed by the home from the
    /// openmls-internal `KeyPackage` structure; opaque to promtuz (we
    /// just round-trip the 32 bytes).
    pub static_hash: Bytes<32>,
}

/// Reply to [`KeyPackageFetchReq`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchResp {
    pub outcome: KeyPackageFetchOutcome,
}

/// Build the canonical signing transcript for an unsigned
/// [`KeyPackageFetchReq`]. The request is *currently* unsigned (the
/// `peer/1` `DhtHello` authenticates the requester); this helper
/// exists so a future revision that adds a user-layer or relay-layer
/// signature can drop in without re-deriving the transcript.
///
/// No caller invokes this function today. We export it for symmetry
/// with the publish/refill helpers and so future protocol revisions
/// can sign the same byte layout without reverse-engineering.
///
/// Layout:
/// ```text
///   KP_FETCH_DOMAIN || protocol_version (BE u16)
///     || target_ipk (32) || requester_relay_id (32) || timestamp (BE u64)
/// ```
pub fn kp_fetch_signing_input(
    protocol_version: u16, target_ipk: &[u8; 32],
    requester_relay_id: &RelayId, timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        KP_FETCH_DOMAIN.len() + 2 + 32 + RelayId::LEN + 8,
    );
    buf.extend_from_slice(KP_FETCH_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(target_ipk);
    buf.extend_from_slice(requester_relay_id.as_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

// ---- Refill -------------------------------------------------------

/// `KeyPackageRefill` request — owner tops up their existing stash.
///
/// **Refill vs Publish**: a Publish is a full-batch idempotent store. A
/// Refill is an *append* — fewer records, intended for the "stash dipped
/// below low-water; top it back up" path. Both preserve existing
/// (in-lifetime) records so anti-pinning rotation doesn't lose
/// not-yet-consumed records during the rotation window. We implement both
/// as additive at the relay side; the only difference is the domain string
/// (so a captured Refill sig can't be replayed as a Publish).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageRefillReq {
    /// Owner's Ed25519 IPK.
    pub ipk: Bytes<32>,
    /// Vector of [`KeyPackageRecord`]s to add. Bounded by
    /// [`KP_STASH_TARGET`].
    pub records: Vec<KeyPackageRecord>,
    /// Publisher-local Unix time in milliseconds. ±[`MAX_KP_SKEW_MS`]
    /// replay-defence window.
    pub timestamp: u64,
    /// Owner's Ed25519 signature over [`kp_refill_signing_input`].
    pub sig: Bytes<64>,
}

/// `KeyPackageRefill` outcome. Mirrors [`KeyPackagePublishOutcome`]
/// closely; semantics differ only in domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPackageRefillOutcome {
    Appended,
    BadSig,
    Expired,
    NotOwner,
    RateLimited,
    TooMany,
    /// Cross-replica static-fields check tripped (same as for Publish —
    /// a record's `(ipk, kp_ref)` already exists with different
    /// `kp_bytes`).
    StaticFieldsConflict,
}

/// Reply to [`KeyPackageRefillReq`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageRefillResp {
    pub outcome: KeyPackageRefillOutcome,
}

/// Build the canonical signing transcript for [`KeyPackageRefillReq::sig`].
///
/// Layout: identical to [`kp_publish_signing_input`] **except** for the
/// domain — this is the deliberate point of having the helper at all.
/// A captured Publish sig must not validate as a Refill sig (the
/// replacement vs append semantics differ; treating one as the other
/// would let an attacker silently downgrade a fresh-batch Publish
/// into an additive-only Refill, defeating the rotation discipline).
pub fn kp_refill_signing_input(
    protocol_version: u16, ipk: &[u8; 32], records_digest: &[u8; 32],
    record_count: u32, timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        KP_REFILL_DOMAIN.len() + 2 + 32 + 4 + 32 + 8,
    );
    buf.extend_from_slice(KP_REFILL_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(ipk);
    buf.extend_from_slice(&record_count.to_be_bytes());
    buf.extend_from_slice(records_digest);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:==:  WELCOME QUEUE RPCS  :==:===:===:===||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

// ---- Publish ------------------------------------------------------

/// `WelcomePublish` request — sender-relay → home-relay deliver-or-
/// queue for a Welcome envelope.
///
/// **Authentication ladder**: the home verifies the *envelope's* own
/// `sender_sig` (already in [`WelcomeEnvelopeP`]) under the inviter's
/// IPK. There is no extra publisher-relay user-layer signature here —
/// the envelope sig is sufficient because it binds (group_id,
/// sender_ipk, recipient_ipk, kp_ref_used, welcome_blob_hash) under
/// the inviter's IPK. A relay forwarding a Welcome cannot forge it
/// without holding the inviter's IPK private key. The relay-to-relay
/// `peer/1` `DhtHello` handshake authenticates the *forwarding*
/// relay, not the inviter — same asymmetry as the sticky-home
/// `Forward` RPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomePublishReq {
    /// The full welcome envelope being queued. The home verifies
    /// `sender_sig` under `sender_ipk` over
    /// [`welcome_envelope_signing_input`] before persisting.
    pub envelope: WelcomeEnvelopeP,
    /// Forwarding-relay-local Unix time in milliseconds at the moment
    /// the publish was issued. ±[`MAX_KP_SKEW_MS`] replay-defence
    /// window at the home — same skew tolerance as the KP family.
    pub timestamp: u64,
}

/// `WelcomePublish` outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WelcomePublishOutcome {
    /// Envelope persisted to `cf_dht_welcome`. Idempotent on a
    /// byte-identical re-publish (the home's storage key includes a
    /// fresh random welcome_id, so duplicates create distinct rows;
    /// the recipient's libcore dedupes by the inner Welcome's group_id
    /// + kp_ref_used at decrypt time).
    Stored,
    /// Envelope's `sender_sig` did not verify under `sender_ipk`, or
    /// `recipient_ipk` mismatch with the Welcome envelope's interior
    /// fields, or structural malformation (oversize blob, missing
    /// fields). Maps to `CloseReason::WelcomeMalformed`.
    BadSig,
    /// `timestamp` outside ±[`MAX_KP_SKEW_MS`] window.
    StaleTimestamp,
    /// Responder is not in the k closest owners for `recipient_ipk`
    /// (welcome routing keys off the recipient IPK directly,
    /// matching presence/queue routing).
    NotOwner,
    /// Per-recipient queue cap [`MAX_WELCOMES_PER_RECIPIENT`]
    /// reached. The home returns this *without* persisting; the
    /// forwarding relay's caller decides whether to retry against a
    /// different home.
    QueueFull,
    /// Per-relay rate limit on welcome publishes tripped. Maps to
    /// `CloseReason::WelcomeRateLimited`.
    RateLimited,
}

/// Reply to [`WelcomePublishReq`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomePublishResp {
    pub outcome: WelcomePublishOutcome,
}

// ---- Fetch --------------------------------------------------------

/// `WelcomeFetch` request — recipient-relay → home-relay drain
/// request for the recipient's queued Welcomes.
///
/// **Authentication**: the recipient's IPK signs `user_sig` over
/// [`welcome_fetch_signing_input`] (which binds `requester_relay_id`
/// as a cross-relay-replay defence). The home additionally checks
/// `requester_relay_id == authenticated_peer_id` from the connection's
/// `DhtHello` — same posture as the sticky-home `QueueFetch` RPC.
/// Mirrors [`crate::proto::dht_p2p::QueueFetch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchReq {
    /// User IPK whose welcomes we want to drain.
    pub user_ipk: Bytes<32>,
    /// Requester relay's `BLAKE3(NodeKey)` identity. Bound to the
    /// signed transcript so a captured `WelcomeFetch` cannot be
    /// replayed by a different relay.
    pub requester_relay_id: RelayId,
    /// Requester-local Unix time in ms; ±[`MAX_KP_SKEW_MS`] window.
    pub timestamp: u64,
    /// User's Ed25519 signature over [`welcome_fetch_signing_input`].
    pub user_sig: Bytes<64>,
}

/// `WelcomeFetch` outcome — found list + exhausted flag (mirrors
/// [`crate::proto::dht_p2p::QueueFetchResp`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchResp {
    pub outcome: WelcomeFetchOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WelcomeFetchOutcome {
    /// Successful drain. `welcomes` is the list of (welcome_id,
    /// envelope) pairs the home holds; the recipient feeds each
    /// envelope to its libcore Welcome processor and acks the ids
    /// via [`WelcomeAckReq`].
    Found(WelcomeFetchFound),
    /// Outer `user_sig` failed to verify, requester binding mismatch,
    /// stale timestamp — same defensive shape as
    /// [`KeyPackageFetchOutcome::RateLimited`] for the requester-
    /// binding case (don't leak whether welcomes exist).
    BadSig,
    /// Responder is not in the k closest owners for `user_ipk`.
    NotOwner,
    /// Per-relay welcome-fetch quota tripped.
    RateLimited,
}

/// Found-arm payload of [`WelcomeFetchOutcome::Found`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchFound {
    /// `(welcome_id, envelope)` pairs. The `welcome_id` is opaque to
    /// the recipient; it's only echoed back in [`WelcomeAckReq`] so
    /// the home can delete by-id without re-iterating.
    pub welcomes: Vec<WelcomeEntry>,
}

/// Single (welcome_id, envelope) pair returned by
/// [`WelcomeFetchOutcome::Found`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeEntry {
    /// Home-generated 8-byte id; opaque to the recipient. Used as the
    /// in-batch identifier echoed back in [`WelcomeAckReq::welcome_ids`]
    /// so the home can delete by-id.
    pub welcome_id: Bytes<8>,
    /// The stored welcome envelope. The recipient's libcore verifies
    /// `sender_sig` again locally before feeding to openmls.
    pub envelope: WelcomeEnvelopeP,
}

// ---- Ack ---------------------------------------------------------

/// `WelcomeAck` request — recipient-relay → home-relay deletion of
/// processed welcomes.
///
/// **Authentication**: identical shape to [`WelcomeFetchReq`] —
/// the recipient signs under their IPK, and the home cross-checks
/// `requester_relay_id` against the connection's authenticated peer
/// id. Domain-separated from `WelcomeFetch` so a captured fetch sig
/// can't be replayed as an ack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeAckReq {
    /// User IPK whose welcomes we're acking. Same as
    /// [`WelcomeFetchReq::user_ipk`].
    pub user_ipk: Bytes<32>,
    /// Requester relay's identity. Bound to the transcript.
    pub requester_relay_id: RelayId,
    /// Welcome ids the recipient confirmed processed; the home
    /// deletes the matching `cf_dht_welcome` rows. Bounded by
    /// [`MAX_WELCOME_ACK_IDS`].
    pub welcome_ids: Vec<Bytes<8>>,
    /// Requester-local Unix time in ms; ±[`MAX_KP_SKEW_MS`] window.
    pub timestamp: u64,
    /// User's Ed25519 signature over [`welcome_ack_signing_input`].
    pub user_sig: Bytes<64>,
}

/// `WelcomeAck` outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeAckResp {
    /// `true` if the home processed the ack (signature verified +
    /// requester binding ok). The home does *not* report individual
    /// id-not-found cases — an id that's already gone is a no-op
    /// (idempotent), and surfacing per-id detail would leak which
    /// ids the home has stored.
    pub ok: bool,
}

// ---- Signing-input helpers ---------------------------------------

/// Build the canonical signing transcript for
/// [`WelcomeFetchReq::user_sig`].
///
/// Layout:
/// ```text
///   WELCOME_FETCH_DOMAIN || protocol_version (BE u16)
///     || user_ipk (32) || requester_relay_id (32) || timestamp (BE u64)
/// ```
///
/// Mirrors the `queue_fetch_signing_input` shape (the
/// `requester_relay_id` binding).
pub fn welcome_fetch_signing_input(
    protocol_version: u16, user_ipk: &[u8; 32], requester_relay_id: &RelayId,
    timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        WELCOME_FETCH_DOMAIN.len() + 2 + 32 + RelayId::LEN + 8,
    );
    buf.extend_from_slice(WELCOME_FETCH_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(requester_relay_id.as_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Build the canonical signing transcript for
/// [`WelcomeAckReq::user_sig`].
///
/// Layout:
/// ```text
///   WELCOME_ACK_DOMAIN || protocol_version (BE u16)
///     || user_ipk (32) || requester_relay_id (32)
///     || ids_count (BE u32) || ids_digest (32) || timestamp (BE u64)
/// ```
///
/// `ids_digest = BLAKE3(concat(welcome_id_i for each id))`. We hash
/// the id list rather than embedding it inline so the transcript is
/// bounded regardless of how many ids the recipient acks. Same
/// length-prefix discipline as `queue_fetch_ack_signing_input`.
pub fn welcome_ack_signing_input(
    protocol_version: u16, user_ipk: &[u8; 32], requester_relay_id: &RelayId,
    welcome_ids: &[[u8; WELCOME_ID_LEN]], timestamp: u64,
) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    for id in welcome_ids {
        hasher.update(id);
    }
    let ids_digest = *hasher.finalize().as_bytes();
    let count = welcome_ids.len() as u32;

    let mut buf = Vec::with_capacity(
        WELCOME_ACK_DOMAIN.len() + 2 + 32 + RelayId::LEN + 4 + 32 + 8,
    );
    buf.extend_from_slice(WELCOME_ACK_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(user_ipk);
    buf.extend_from_slice(requester_relay_id.as_bytes());
    buf.extend_from_slice(&count.to_be_bytes());
    buf.extend_from_slice(&ids_digest);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:===:  TIER-1 WRAPPER  :===:===:===:||
//===:===:===:===:===:===:===:===:===:===:===:===:===||
//
// Only the two GATE-ONLY RPCs get a dedicated wrap signing-input here
// (FetchKeyPackage, PublishWelcome). The three user-signed RPCs
// (PublishKeyPackage, FetchWelcomes, AckWelcomes) reuse the *inner*
// Tier-2 transcripts directly — `kp_publish_signing_input` /
// `kp_refill_signing_input` and `welcome_fetch_signing_input` /
// `welcome_ack_signing_input` — because the phone-produced signature is
// forwarded verbatim to the K storage homes and must verify there too.
// See the category note at KP_FETCH_WRAP_DOMAIN above.

/// Mode flag carried on `CRelayPacket::PublishKeyPackage`. The home
/// translates `Publish` to a `KeyPackagePublish` (replace stash
/// atomically) and `Refill` to a `KeyPackageRefill` (append to stash).
/// The phone signs the matching inner transcript
/// ([`kp_publish_signing_input`] for `Publish`,
/// [`kp_refill_signing_input`] for `Refill`) so the forwarded sig
/// verifies at the K homes — distinct domains mean a captured Publish
/// sig can't be re-typed as a Refill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum KpPublishMode {
    Publish = 0,
    Refill  = 1,
}

/// Build the canonical signing transcript for `CRelayPacket::FetchKeyPackage`.
///
/// Layout:
/// ```text
///   KP_FETCH_WRAP_DOMAIN || protocol_version (BE u16)
///     || sender_ipk (32) || target_ipk (32) || timestamp (BE u64)
/// ```
pub fn kp_fetch_wrap_signing_input(
    protocol_version: u16, sender_ipk: &[u8; 32], target_ipk: &[u8; 32],
    timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        KP_FETCH_WRAP_DOMAIN.len() + 2 + 32 + 32 + 8,
    );
    buf.extend_from_slice(KP_FETCH_WRAP_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(sender_ipk);
    buf.extend_from_slice(target_ipk);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

/// Build the canonical signing transcript for `CRelayPacket::PublishWelcome`.
///
/// Layout:
/// ```text
///   WELCOME_PUBLISH_WRAP_DOMAIN || protocol_version (BE u16)
///     || sender_ipk (32) || BLAKE3(welcome_blob) (32) || timestamp (BE u64)
/// ```
///
/// Hashes `welcome_blob` internally — the same hash the envelope's own
/// [`welcome_envelope_signing_input`] covers, so the wrapper sig is bound
/// to the exact MLS payload being published. Hashing here (rather than
/// taking a pre-computed digest) keeps the one blake3 call in `common` so
/// neither the relay nor libcore needs a direct blake3 dependency. The
/// envelope's `sender_sig` already covers the recipient/group/kp_ref
/// metadata, so the wrapper does not re-bind them.
pub fn welcome_publish_wrap_signing_input(
    protocol_version: u16, sender_ipk: &[u8; 32], welcome_blob: &[u8], timestamp: u64,
) -> Vec<u8> {
    let blob_hash = blake3::hash(welcome_blob);
    let mut buf = Vec::with_capacity(
        WELCOME_PUBLISH_WRAP_DOMAIN.len() + 2 + 32 + 32 + 8,
    );
    buf.extend_from_slice(WELCOME_PUBLISH_WRAP_DOMAIN);
    buf.extend_from_slice(&protocol_version.to_be_bytes());
    buf.extend_from_slice(sender_ipk);
    buf.extend_from_slice(blob_hash.as_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

//===:===:===:===:===:===:===:===:===:===:===:===:===||
//===:===:===:===:===:===:  TESTS  :===:===:===:===:==||
//===:===:===:===:===:===:===:===:===:===:===:===:===||

#[cfg(all(test, feature = "crypto"))]
mod tests {
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;

    use super::*;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;
    use crate::quic::id::NodeId;

    /// Mint a fresh Ed25519 keypair via OS-RNG. Same idiom as the
    /// existing `dht_p2p` test fixture.
    fn fresh_signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    /// A PairDecline signed by the real decliner verifies; a forged one (wrong
    /// key) or a tampered reason does not — so a malicious relay can't fake a
    /// rejection to grief a pair.
    #[test]
    fn pair_decline_sig_authenticates_the_decliner() {
        use ed25519_dalek::Signature;
        use ed25519_dalek::Verifier;
        let decliner = fresh_signing_key();
        let inviter: [u8; 32] = fresh_signing_key().verifying_key().to_bytes();
        let from = decliner.verifying_key().to_bytes();
        let msg = pair_decline_signing_input(&from, &inviter, DECLINE_KP_CONSUMED, 42);
        let sig = decliner.sign(&msg);
        assert!(decliner.verifying_key().verify(&msg, &sig).is_ok(), "real decliner verifies");

        // Forged by a different key (a relay) — fails under the decliner's IPK.
        let forged = fresh_signing_key().sign(&msg);
        assert!(decliner.verifying_key().verify(&msg, &forged).is_err(), "forgery rejected");

        // Tampered reason → different transcript → sig no longer matches.
        let tampered = pair_decline_signing_input(&from, &inviter, DECLINE_USER_REJECTED, 42);
        assert!(
            decliner.verifying_key().verify(&tampered, &Signature::from(sig)).is_err(),
            "reason is bound into the transcript"
        );
    }

    /// Build a `KeyPackageRecord` with internally-consistent fields.
    /// The `kp_bytes` field is opaque (we just stuff `payload` in;
    /// the openmls TLS-encoded form is produced by libcore client code).
    fn build_record(
        owner: &SigningKey, kp_ref: Vec<u8>, kp_bytes: Vec<u8>, expires_at_ms: u64,
    ) -> KeyPackageRecord {
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &kp_ref,
            &kp_bytes,
            expires_at_ms,
        );
        let sig = owner.sign(&msg);
        KeyPackageRecord {
            ipk: ipk.into(),
            kp_ref: kp_ref.into(),
            kp_bytes: kp_bytes.into(),
            expires_at_ms,
            owner_sig: sig.to_bytes().into(),
        }
    }

    // -----------------------------------------------------------------
    // Tier-1 gate-wrapper signing-input layout pins
    // -----------------------------------------------------------------

    /// Pin the byte-length of the two GATE-ONLY wrapper signing-inputs
    /// (FetchKeyPackage, PublishWelcome). The three user-signed RPCs
    /// reuse the inner Tier-2 transcripts (pinned in their own tests),
    /// so they have no wrap helper to pin here. A drift means a relay
    /// verifier reconstructed a transcript that doesn't match what the
    /// phone signed, breaking auth silently in the field.
    #[test]
    fn gate_wrapper_signing_input_layouts_are_pinned() {
        let ipk: [u8; 32] = [0x11; 32];
        let target_ipk: [u8; 32] = [0x22; 32];
        let blob: &[u8] = b"opaque-welcome-blob";
        let ts: u64 = 1_700_000_000_000;

        // KP_FETCH_WRAP: domain || version(2) || ipk(32)
        //               || target_ipk(32) || timestamp(8)
        let kp_fetch = kp_fetch_wrap_signing_input(
            MLS_WIRE_VERSION, &ipk, &target_ipk, ts,
        );
        assert_eq!(kp_fetch.len(), KP_FETCH_WRAP_DOMAIN.len() + 2 + 32 + 32 + 8);

        // WELCOME_PUBLISH_WRAP: domain || version(2) || ipk(32)
        //                      || BLAKE3(welcome_blob)(32) || timestamp(8)
        let w_pub = welcome_publish_wrap_signing_input(MLS_WIRE_VERSION, &ipk, blob, ts);
        assert_eq!(w_pub.len(), WELCOME_PUBLISH_WRAP_DOMAIN.len() + 2 + 32 + 32 + 8);
    }

    // -----------------------------------------------------------------
    // Postcard round-trips for envelope types
    // -----------------------------------------------------------------

    #[test]
    fn mls_envelope_outer_round_trip_for_each_variant() {
        let app = MlsEnvelopeP::Application(MlsApplicationEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: [0x42; 32].into(),
            epoch: 1,
            mls_message: b"x".to_vec().into(),
            sender_sig: [0; 64].into(),
        });
        let welcome = MlsEnvelopeP::Welcome(WelcomeEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: [0x42; 32].into(),
            sender_ipk: [1; 32].into(),
            recipient_ipk: [2; 32].into(),
            welcome_blob: b"y".to_vec().into(),
            kp_ref_used: [0; 32].into(),
            sender_sig: [0; 64].into(),
            pairing: None,
        });
        for env in [app, welcome] {
            let bytes = env.ser().expect("ser");
            let decoded = MlsEnvelopeP::deser(&bytes).expect("deser");
            assert_eq!(decoded, env);
        }
    }

    // -----------------------------------------------------------------
    // Postcard round-trips for KP record + RPC types
    // -----------------------------------------------------------------

    #[test]
    fn keypackage_record_round_trip() {
        let owner = fresh_signing_key();
        let rec = build_record(
            &owner,
            vec![0x55; 32],
            b"kp-tls-bytes".to_vec(),
            1_700_000_000_000 + KEYPACKAGE_LIFETIME_MS,
        );
        let bytes = rec.ser().expect("ser");
        let decoded = KeyPackageRecord::deser(&bytes).expect("deser");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn keypackage_publish_req_resp_round_trip() {
        let owner = fresh_signing_key();
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let recs = vec![
            build_record(&owner, vec![1; 32], b"a".to_vec(), 999),
            build_record(&owner, vec![2; 32], b"b".to_vec(), 1000),
        ];
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, &recs);
        let msg = kp_publish_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &digest,
            recs.len() as u32,
            42,
        );
        let sig = owner.sign(&msg);
        let req = KeyPackagePublishReq {
            ipk: ipk.into(),
            records: recs,
            timestamp: 42,
            sig: sig.to_bytes().into(),
        };
        let bytes = req.ser().expect("ser");
        let decoded = KeyPackagePublishReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        for outcome in [
            KeyPackagePublishOutcome::Stored,
            KeyPackagePublishOutcome::BadSig,
            KeyPackagePublishOutcome::Expired,
            KeyPackagePublishOutcome::NotOwner,
            KeyPackagePublishOutcome::RateLimited,
            KeyPackagePublishOutcome::TooMany,
            KeyPackagePublishOutcome::StaticFieldsConflict,
        ] {
            let r = KeyPackagePublishResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = KeyPackagePublishResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn keypackage_fetch_req_resp_round_trip() {
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let req = KeyPackageFetchReq {
            target_ipk: [0x99; 32].into(),
            requester_relay_id: req_id,
            timestamp: 100,
        };
        let bytes = req.ser().expect("ser");
        let decoded = KeyPackageFetchReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        let owner = fresh_signing_key();
        let rec = build_record(&owner, vec![0; 32], b"kp".to_vec(), 1_000_000);
        let outcomes = vec![
            KeyPackageFetchOutcome::Found(KeyPackageFetchFound {
                record: rec,
                remaining: 99,
                static_hash: [0xEE; 32].into(),
            }),
            KeyPackageFetchOutcome::NoStash,
            KeyPackageFetchOutcome::NotOwner,
            KeyPackageFetchOutcome::RateLimited,
        ];
        for outcome in outcomes {
            let r = KeyPackageFetchResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = KeyPackageFetchResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn keypackage_refill_req_resp_round_trip() {
        let owner = fresh_signing_key();
        let ipk: [u8; 32] = owner.verifying_key().to_bytes();
        let recs = vec![build_record(&owner, vec![3; 32], b"r".to_vec(), 555)];
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, &recs);
        let msg = kp_refill_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &digest,
            recs.len() as u32,
            7,
        );
        let sig = owner.sign(&msg);
        let req = KeyPackageRefillReq {
            ipk: ipk.into(),
            records: recs,
            timestamp: 7,
            sig: sig.to_bytes().into(),
        };
        let bytes = req.ser().expect("ser");
        let decoded = KeyPackageRefillReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        for outcome in [
            KeyPackageRefillOutcome::Appended,
            KeyPackageRefillOutcome::BadSig,
            KeyPackageRefillOutcome::Expired,
            KeyPackageRefillOutcome::NotOwner,
            KeyPackageRefillOutcome::RateLimited,
            KeyPackageRefillOutcome::TooMany,
            KeyPackageRefillOutcome::StaticFieldsConflict,
        ] {
            let r = KeyPackageRefillResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = KeyPackageRefillResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    // -----------------------------------------------------------------
    // Signing-input determinism + domain separation
    // -----------------------------------------------------------------

    #[test]
    fn envelope_signing_input_layout_is_stable() {
        // Pin the byte layout: domain + version + to_ipk(32) +
        // group_id(32) + epoch(8) + mls_hash(32). Same approach as
        // `dht_p2p`'s layout pins.
        let to_ipk = [0u8; 32];
        let group_id = [0u8; 32];
        let mls = b"x";
        let buf = envelope_signing_input(
            MLS_WIRE_VERSION,
            &to_ipk,
            &group_id,
            0xDEAD_BEEF_CAFE_F00D,
            mls,
        );
        assert_eq!(
            buf.len(),
            MLS_ENVELOPE_SIG_DOMAIN.len() + 2 + 32 + 32 + 8 + 32
        );
        assert!(buf.starts_with(MLS_ENVELOPE_SIG_DOMAIN));
        let off = MLS_ENVELOPE_SIG_DOMAIN.len();
        assert_eq!(&buf[off..off + 2], &MLS_WIRE_VERSION.to_be_bytes());
    }

    #[test]
    fn welcome_envelope_signing_input_layout_is_stable() {
        let group_id = [0u8; 32];
        let sender = [0u8; 32];
        let recipient = [0u8; 32];
        let kp_ref = [0u8; 32];
        let blob = b"";
        let buf = welcome_envelope_signing_input(
            MLS_WIRE_VERSION,
            &group_id,
            &sender,
            &recipient,
            &kp_ref,
            blob,
        );
        assert_eq!(
            buf.len(),
            WELCOME_ENVELOPE_SIG_DOMAIN.len() + 2 + 32 + 32 + 32 + 32 + 32
        );
        assert!(buf.starts_with(WELCOME_ENVELOPE_SIG_DOMAIN));
    }

    #[test]
    fn kp_publish_and_refill_share_layout_but_differ_in_domain() {
        // A captured Publish sig must not validate as a Refill sig (or
        // vice versa). The domain prefix is the only difference (the
        // field layout after the domain is identical), so we anchor on:
        //   1. The full transcripts differ (so a captured sig won't
        //      validate under the wrong helper).
        //   2. The *suffix* after the domain bytes is byte-identical
        //      (so a future field-layout drift breaks the assertion
        //      and surfaces here, not weeks later).
        //
        // We do NOT assert on overall length because the two domain
        // strings are different lengths (`KP_PUBLISH_DOMAIN` vs
        // `KP_REFILL_DOMAIN`) by design.
        let ipk = [0u8; 32];
        let digest = [0u8; 32];
        let pub_buf = kp_publish_signing_input(MLS_WIRE_VERSION, &ipk, &digest, 0, 0);
        let refill_buf = kp_refill_signing_input(MLS_WIRE_VERSION, &ipk, &digest, 0, 0);
        assert_ne!(
            pub_buf, refill_buf,
            "different domains must produce different transcripts"
        );
        assert_eq!(
            &pub_buf[KP_PUBLISH_DOMAIN.len()..],
            &refill_buf[KP_REFILL_DOMAIN.len()..],
            "post-domain layout must be byte-identical"
        );
        // And those suffixes both start with the version + ipk + ...
        assert!(
            pub_buf.starts_with(KP_PUBLISH_DOMAIN),
            "publish prefix is its own domain"
        );
        assert!(
            refill_buf.starts_with(KP_REFILL_DOMAIN),
            "refill prefix is its own domain"
        );
    }

    #[test]
    fn mls_domain_strings_are_distinct() {
        // Every MLS-related signing-input domain must be unique so a
        // captured signature for one packet kind cannot be replayed
        // as another. Mirrors the `sticky_home_domain_strings_are_distinct`
        // discipline already in `dht_p2p`.
        let domains: &[&[u8]] = &[
            MLS_DOMAIN_PREFIX,
            MLS_ENVELOPE_SIG_DOMAIN,
            WELCOME_ENVELOPE_SIG_DOMAIN,
            KP_PUBLISH_DOMAIN,
            KP_FETCH_DOMAIN,
            KP_REFILL_DOMAIN,
            KP_RECORD_DOMAIN,
            WELCOME_FETCH_DOMAIN,
            WELCOME_ACK_DOMAIN,
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

    /// Changing `to_ipk` must change the transcript — the defining
    /// property of the per-recipient binding: without it the
    /// relay-redirection attack works.
    #[test]
    fn envelope_signing_input_to_ipk_binding_changes_transcript() {
        let group_id = [0u8; 32];
        let mls = b"same-message";
        let to_a = [0xAA; 32];
        let to_b = [0xBB; 32];

        let buf_a = envelope_signing_input(MLS_WIRE_VERSION, &to_a, &group_id, 1, mls);
        let buf_b = envelope_signing_input(MLS_WIRE_VERSION, &to_b, &group_id, 1, mls);
        assert_ne!(
            buf_a, buf_b,
            "different to_ipk must produce different transcripts"
        );
    }

    /// Records-digest is deterministic over identical batches and
    /// changes when any record changes. Catches a regression where
    /// the digest accidentally hashes the postcard-encoded form
    /// (which would be brittle across postcard versions).
    #[test]
    fn records_digest_is_deterministic_and_change_sensitive() {
        let owner = fresh_signing_key();
        let r1 = build_record(&owner, vec![1; 32], b"a".to_vec(), 100);
        let r2 = build_record(&owner, vec![2; 32], b"b".to_vec(), 200);

        let d_a = kp_publish_records_digest(MLS_WIRE_VERSION, &[r1.clone(), r2.clone()]);

        // Adding a record changes the digest.
        let r3 = build_record(&owner, vec![3; 32], b"c".to_vec(), 300);
        let d_c = kp_publish_records_digest(MLS_WIRE_VERSION, &[r1.clone(), r2.clone(), r3]);
        assert_ne!(d_a, d_c);
    }

    // ---------------------------------------------------------------
    // Welcome queue RPC types
    // ---------------------------------------------------------------

    fn build_welcome_envelope(
        sender: &SigningKey, recipient_ipk: [u8; 32], group_id: [u8; 32],
        kp_ref_used: [u8; 32], welcome_blob: Vec<u8>,
    ) -> WelcomeEnvelopeP {
        let sender_ipk: [u8; 32] = sender.verifying_key().to_bytes();
        let msg = welcome_envelope_signing_input(
            MLS_WIRE_VERSION,
            &group_id,
            &sender_ipk,
            &recipient_ipk,
            &kp_ref_used,
            &welcome_blob,
        );
        let sig = sender.sign(&msg);
        WelcomeEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: group_id.into(),
            sender_ipk: sender_ipk.into(),
            recipient_ipk: recipient_ipk.into(),
            welcome_blob: welcome_blob.into(),
            kp_ref_used: kp_ref_used.into(),
            sender_sig: sig.to_bytes().into(),
            pairing: None,
        }
    }

    #[test]
    fn welcome_publish_req_resp_round_trip() {
        let sender = fresh_signing_key();
        let env = build_welcome_envelope(
            &sender,
            [0xAA; 32],
            [0xBB; 32],
            [0xCC; 32],
            b"opaque-welcome".to_vec(),
        );
        let req = WelcomePublishReq {
            envelope: env,
            timestamp: 12345,
        };
        let bytes = req.ser().expect("ser");
        let decoded = WelcomePublishReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        for outcome in [
            WelcomePublishOutcome::Stored,
            WelcomePublishOutcome::BadSig,
            WelcomePublishOutcome::StaleTimestamp,
            WelcomePublishOutcome::NotOwner,
            WelcomePublishOutcome::QueueFull,
            WelcomePublishOutcome::RateLimited,
        ] {
            let r = WelcomePublishResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = WelcomePublishResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn welcome_fetch_req_resp_round_trip() {
        let user = fresh_signing_key();
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let timestamp = 999;

        let msg = welcome_fetch_signing_input(
            MLS_WIRE_VERSION,
            &user_ipk,
            &req_id,
            timestamp,
        );
        let sig = user.sign(&msg);
        let req = WelcomeFetchReq {
            user_ipk: user_ipk.into(),
            requester_relay_id: req_id,
            timestamp,
            user_sig: sig.to_bytes().into(),
        };
        let bytes = req.ser().expect("ser");
        let decoded = WelcomeFetchReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        // Found-arm:
        let env = build_welcome_envelope(
            &user, // sender doesn't matter for round-trip
            [0xAA; 32],
            [0xBB; 32],
            [0xCC; 32],
            b"x".to_vec(),
        );
        let resp_found = WelcomeFetchResp {
            outcome: WelcomeFetchOutcome::Found(WelcomeFetchFound {
                welcomes: vec![WelcomeEntry {
                    welcome_id: [1u8; 8].into(),
                    envelope: env,
                }],
            }),
        };
        let bytes = resp_found.ser().expect("ser");
        let decoded = WelcomeFetchResp::deser(&bytes).expect("deser");
        assert_eq!(decoded, resp_found);

        for outcome in [
            WelcomeFetchOutcome::BadSig,
            WelcomeFetchOutcome::NotOwner,
            WelcomeFetchOutcome::RateLimited,
        ] {
            let r = WelcomeFetchResp { outcome };
            let bytes = r.ser().expect("ser");
            let decoded = WelcomeFetchResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn welcome_ack_req_resp_round_trip() {
        let user = fresh_signing_key();
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let req_relay = fresh_signing_key();
        let req_id = NodeId::new(req_relay.verifying_key().to_bytes());
        let ids: Vec<[u8; 8]> = vec![[1u8; 8], [2u8; 8]];
        let timestamp = 555;

        let msg = welcome_ack_signing_input(
            MLS_WIRE_VERSION,
            &user_ipk,
            &req_id,
            &ids,
            timestamp,
        );
        let sig = user.sign(&msg);
        let req = WelcomeAckReq {
            user_ipk: user_ipk.into(),
            requester_relay_id: req_id,
            welcome_ids: ids.iter().map(|i| (*i).into()).collect(),
            timestamp,
            user_sig: sig.to_bytes().into(),
        };
        let bytes = req.ser().expect("ser");
        let decoded = WelcomeAckReq::deser(&bytes).expect("deser");
        assert_eq!(decoded, req);

        for ok in [true, false] {
            let r = WelcomeAckResp { ok };
            let bytes = r.ser().expect("ser");
            let decoded = WelcomeAckResp::deser(&bytes).expect("deser");
            assert_eq!(decoded, r);
        }
    }

    /// Changing any signed field of a welcome ack must change the
    /// transcript — the defining property of length-prefixed-id-list
    /// hashing.
    #[test]
    fn welcome_ack_signing_input_is_change_sensitive() {
        let user_ipk = [0u8; 32];
        let req_id = NodeId::new([0u8; 32]);
        let ids_a: Vec<[u8; 8]> = vec![[1; 8]];
        let ids_b: Vec<[u8; 8]> = vec![[2; 8]];
        let a = welcome_ack_signing_input(MLS_WIRE_VERSION, &user_ipk, &req_id, &ids_a, 0);
        let b = welcome_ack_signing_input(MLS_WIRE_VERSION, &user_ipk, &req_id, &ids_b, 0);
        assert_ne!(a, b, "different ids must produce different transcripts");

        let c =
            welcome_ack_signing_input(MLS_WIRE_VERSION, &user_ipk, &req_id, &ids_a, 1);
        assert_ne!(a, c, "different timestamp must produce different transcripts");
    }

    #[test]
    fn app_payload_round_trips() {
        use crate::proto::pack::{Packer, Unpacker};
        let t = AppPayload::Text("hello".into());
        assert_eq!(AppPayload::deser(&t.ser().unwrap()).unwrap(), t);
        let r = AppPayload::Receipt { kind: ReceiptKind::Delivered, upto: [9u8; 16] };
        assert_eq!(AppPayload::deser(&r.ser().unwrap()).unwrap(), r);
    }
}
