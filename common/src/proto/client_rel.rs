//! Client to Relay Proto

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::PROTOCOL_VERSION;
use crate::proto::Sender;
use crate::types::bytes::ByteVec;
use crate::types::bytes::Bytes;

/// Domain separator for the dispatch signature. Bumping the suffix is a
/// breaking protocol change; both client and relay must agree exactly.
pub const DISPATCH_SIG_DOMAIN: &[u8] = b"promtuz-dispatch-v1";

/// Build the canonical bytes signed/verified for a `DispatchP`.
///
/// Layout: `DISPATCH_SIG_DOMAIN || PROTOCOL_VERSION_BE || to || from || id || payload`
pub fn dispatch_sig_message(
    to: &[u8; 32], from: &[u8; 32], id: &[u8; 16], payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        DISPATCH_SIG_DOMAIN.len() + 2 + to.len() + from.len() + id.len() + payload.len(),
    );
    buf.extend_from_slice(DISPATCH_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(to);
    buf.extend_from_slice(from);
    buf.extend_from_slice(id);
    buf.extend_from_slice(payload);
    buf
}

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:==: HANDSHAKE :==:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ServerHandshakeResultP {
    Accept {
        timestamp: u64,
        /// This relay's DHT NodeId (`BLAKE3(NodeKey)`),
        /// or `None` when the relay has DHT disabled. The phone binds it
        /// as `requester_relay_id` when signing the
        /// `FetchWelcomes` / `AckWelcomes` wrappers (the inner Tier-2
        /// `welcome_fetch/ack_signing_input` transcript names the
        /// requesting relay). Absent ⇒ welcome fetch/ack can't be
        /// signed for this home, which is fine because a DHT-disabled
        /// relay replies `DhtUnavailable` to those RPCs anyway.
        relay_node_id: Option<Bytes<32>>,
    },
    Reject { reason: String },
}

/// Client Handshake Packet
///
/// Handshake initiates from Client
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum CHandshakePacket {
    Hello { ipk: Bytes<32> },
    Proof { sig: Bytes<64> },
}

/// Server Handshake Packet
///
/// Server's response to client handshake
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SHandshakePacket {
    Challenge { nonce: Bytes<32> },
    HandshakeResult(ServerHandshakeResultP),
}

#[cfg(feature = "client")]
impl Sender for CHandshakePacket {}

#[cfg(feature = "server")]
impl Sender for SHandshakePacket {}

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: QUERIES :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum QueryP {
    PubAddress,
    // room to grow
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum QueryResultP {
    PubAddress { addr: SocketAddr },
    NotFound,
    Error { reason: String },
}

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: FORWARD :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

/// Client → Relay
///
/// `sig` covers (in order, no separators):
///   `b"promtuz-dispatch-v1"`
///   || `PROTOCOL_VERSION.to_be_bytes()` (u16, big-endian)
///   || `to`      (32 bytes)
///   || `from`    (32 bytes)
///   || `id`      (16 bytes — UUIDv7 minted by the *sender*)
///   || `payload` (ciphertext bytes)
///
/// The relay verifies that `from == authenticated session identity` AND that
/// the signature above validates under `from`. The `id` is signed by the
/// client, never minted by the relay, so it survives forward-and-store as
/// authenticated metadata.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DispatchP {
    pub to:      Bytes<32>,
    pub from:    Bytes<32>,
    /// Sender-authoritative dispatch_id (monotonic 16B, from the sender's
    /// `messages.dispatch_id`); promoted to `DeliverP::id` unchanged. The
    /// recipient dedups on it, so a retry re-sends the same id.
    pub id:      Bytes<16>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
}

/// Relay → Client (relay-verified delivery)
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DeliverP {
    /// Sender's dispatch_id, copied verbatim from `DispatchP::id`. The
    /// recipient dedups on `(from, id)`.
    pub id:      Bytes<16>,
    pub from:    Bytes<32>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
}

/// Activity bits for [`EphemeralP::activity`]. OR them for "several at once".
/// `0` = present-but-idle (a bare presence heartbeat).
pub const ACTIVITY_TYPING: u16 = 1 << 0;
pub const ACTIVITY_RECORDING: u16 = 1 << 1;
pub const ACTIVITY_CHOOSING_STICKER: u16 = 1 << 2;
pub const ACTIVITY_CHOOSING_EMOJI: u16 = 1 << 3;
pub const ACTIVITY_UPLOADING: u16 = 1 << 4;

/// Domain separator for the ephemeral-signal signature.
pub const EPHEMERAL_SIG_DOMAIN: &[u8] = b"promtuz-ephemeral-v1";

/// Ephemeral peer signal (presence heartbeat / typing / recording / …).
/// Cleartext and relay-routed (NOT MLS): the relay delivers it only if the
/// recipient is online and DROPS it otherwise — never queued. Authenticated by
/// `sig` under `from`, so the recipient (and relay) can reject a forged signal.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct EphemeralP {
    pub to:        Bytes<32>,
    pub from:      Bytes<32>,
    /// OR of `ACTIVITY_*` bits; `0` = present-but-idle.
    pub activity:  u16,
    pub timestamp: u64,
    pub sig:       Bytes<64>,
}

/// Canonical bytes signed/verified for an [`EphemeralP`].
/// Layout: `EPHEMERAL_SIG_DOMAIN || PROTOCOL_VERSION_BE || to || from || activity_be || timestamp_be`
pub fn ephemeral_sig_message(
    to: &[u8; 32], from: &[u8; 32], activity: u16, timestamp: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(EPHEMERAL_SIG_DOMAIN.len() + 2 + 32 + 32 + 2 + 8);
    buf.extend_from_slice(EPHEMERAL_SIG_DOMAIN);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(to);
    buf.extend_from_slice(from);
    buf.extend_from_slice(&activity.to_be_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum DispatchAckP {
    Queued,
    Delivered,
    NotFound,
    InvalidSig,
    /// Recipient's per-user RocksDB queue is at capacity. Sender should back
    /// off; the message was *not* stored. See
    /// `relay::storage::MAX_QUEUED_PER_RECIPIENT`.
    QueueFull,
    /// Recipient was offline locally and the dispatch was successfully
    /// queued at ≥ K_MIN of the recipient's K-closest "home" relays via
    /// the sticky-home DHT-forward path. Distinct from [`Self::Queued`]
    /// (which is the local-only fallback) so the sender knows the
    /// dispatch is held by a deterministic K-relay set keyed off the
    /// recipient's IPK rather than only on the originating relay.
    ///
    /// The dispatch is queued at K_MIN homes; eventual delivery depends on
    /// the recipient draining one of those homes on reconnect. Sender
    /// has no further proof of delivery — read receipts are out of scope.
    Forwarded,
    Error { reason: String },
}

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: RELAY-P :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

/// Client Relay Packet
///
/// Packets sent from Client to Server
///
/// CLIENT --> SERVER
// ponytail: wire enum — always heap-packed into a Vec before send and
// dropped right after dispatch, so the inline size gap doesn't matter.
// Boxing the large `PublishWelcome` field would thread `.into()`/deref
// through every construct/match site for no wire-format change.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum CRelayPacket {
    Query(QueryP),
    Dispatch(DispatchP),

    /// Fire-and-forget ephemeral signal (presence/typing). The relay routes it
    /// to the recipient if online and drops it otherwise; no reply.
    Ephemeral(EphemeralP),

    /// User acknowledges receiving valid delivery of messages
    DeliverAck,

    /// Drains Queue, user requesting for all incoming messages
    DrainQueue,
    /// User confirms storing messages hence queue can be cleared from server
    AckDrain,

    /// Sticky-home — user-signed authorisation for the relay
    /// to drain the user's queues from K-closest "home" relays on the
    /// user's behalf.
    ///
    /// The relay R_r the user just authenticated to is *not always* in
    /// the user's K-closest set; in that case R_r must impersonate the
    /// user when issuing `QueueFetch` against the K homes. The home relay
    /// only ships queued dispatches when the user has signed the request —
    /// `sig` is that user signature, sized so a single sign-on serves all K
    /// homes in the recipient's set (the transcript binds the user, the
    /// requesting relay, and a freshness timestamp; it does **not**
    /// bind the home being addressed, so one signature works for every
    /// home in that set).
    ///
    /// Transcript: [`crate::proto::dht_p2p::queue_fetch_signing_input`]
    /// over `(self_ipk, current_relay_id, timestamp)`. The relay buffers
    /// `(timestamp, sig)` on its `ClientContext` and presents them as
    /// `QueueFetch.user_sig` when fanning out to homes.
    ///
    /// This packet is sent at the *fetch* end of the recipient flow.
    /// The companion `QueueFetchAck` deletion path (proving the user
    /// received specific dispatch ids) is handled separately by
    /// [`Self::AckAuth`] — a transcript over `delivered_ids` requires
    /// the relay to know the id list before it can ask libcore to sign,
    /// which is impossible before fetching has happened.
    DrainAuth { timestamp: u64, sig: Bytes<64> },

    /// Sticky-home — user-signed authorisation for the K
    /// home relays to GC the listed dispatch ids from their
    /// `cf_dht_queue` after the relay successfully delivered them to
    /// this client.
    ///
    /// Sent in response to [`SRelayPacket::AckAuthRequest`] (the relay
    /// asks libcore to sign the union of delivered ids drawn from all
    /// K homes). The transcript is
    /// [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`] over
    /// `(self_ipk, requester_relay_id, delivered_ids, timestamp)` —
    /// the same signature serves all K homes (the homes verify the
    /// transcript byte-for-byte and only delete the ids they actually
    /// hold from the union list), but each home additionally rejects
    /// the RPC at the handler layer if `requester_relay_id` does not
    /// match the connection's authenticated `DhtHello` peer id. This
    /// is the cross-relay replay defense (an ack the user signed for
    /// relay R_a is no longer redirectable to a different home via a
    /// different relay R_b).
    ///
    /// **Why the user signs (not the relay)**: a malicious relay could
    /// otherwise forge an ack and force every home to drop a user's
    /// queued messages without delivery. Routing the ack through the
    /// user's IPK signature mirrors the existing `DrainAuth` design
    /// for the fetch direction.
    AckAuth { sig: Bytes<64>, timestamp: u64 },

    /// Libcore wrapper around `KeyPackagePublish` (when `mode = Publish`)
    /// or `KeyPackageRefill` (when `mode = Refill`). **User-signed RPC**:
    /// `sig` is the phone's signature over the *inner* Tier-2 transcript
    /// ([`crate::proto::mls_wire::kp_publish_signing_input`] /
    /// [`crate::proto::mls_wire::kp_refill_signing_input`], selected by
    /// `mode`), bound to `(ipk, records-digest, timestamp)`. The home
    /// verifies it against the connection-authenticated IPK (its gate)
    /// and forwards the *same* signature inside the
    /// `KeyPackagePublishReq`/`RefillReq` to the K=3 DHT homes, which
    /// re-verify it — the home is a forwarder, never a trust root.
    /// Reply: [`SRelayPacket::KeyPackagePublished`] or
    /// [`SRelayPacket::DhtUnavailable`].
    PublishKeyPackage {
        records:   Vec<crate::proto::mls_wire::KeyPackageRecord>,
        timestamp: u64,
        mode:      crate::proto::mls_wire::KpPublishMode,
        sig:       Bytes<64>,
    },

    /// Libcore wrapper around `KeyPackageFetch`. **Gate-only RPC**: the
    /// inner `KeyPackageFetchReq` carries no user sig (it's
    /// DhtHello-authenticated relay-to-relay), so `sig` is a wrapper-gate
    /// signature over
    /// [`crate::proto::mls_wire::kp_fetch_wrap_signing_input`] that the
    /// home verifies locally for freshness + attribution and does not
    /// propagate. Reply: [`SRelayPacket::KeyPackageFetched`] or
    /// [`SRelayPacket::DhtUnavailable`].
    FetchKeyPackage {
        target_ipk: Bytes<32>,
        timestamp:  u64,
        sig:        Bytes<64>,
    },

    /// Libcore wrapper around Welcome publish to the recipient's K=3
    /// homes. **Gate-only RPC**: the user authorization rides inside
    /// `envelope.sender_sig` (forwarded intact); `sig` is a wrapper-gate
    /// signature over
    /// [`crate::proto::mls_wire::welcome_publish_wrap_signing_input`]
    /// proving this authenticated phone asked to publish now. Reply:
    /// [`SRelayPacket::WelcomePublished`] or
    /// [`SRelayPacket::DhtUnavailable`].
    PublishWelcome {
        envelope:  crate::proto::mls_wire::WelcomeEnvelopeP,
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// Libcore wrapper around Welcome drain of the *calling IPK's own*
    /// queue from its K=3 homes. **User-signed RPC**: `sig` is the
    /// phone's signature over the inner
    /// [`crate::proto::mls_wire::welcome_fetch_signing_input`], bound to
    /// `(user_ipk, requester_relay_id = home NodeId, timestamp)` — the
    /// phone learns its home's NodeId from the client/0 handshake. The
    /// home verifies, then forwards the same sig (with its own NodeId as
    /// `requester_relay_id`) to the K homes. Reply:
    /// [`SRelayPacket::WelcomesFetched`] or
    /// [`SRelayPacket::DhtUnavailable`].
    FetchWelcomes {
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// Libcore wrapper around Welcome ack; the K homes GC the listed
    /// `welcome_ids`. **User-signed RPC**: `sig` is the phone's signature
    /// over the inner
    /// [`crate::proto::mls_wire::welcome_ack_signing_input`], bound to
    /// `(user_ipk, requester_relay_id = home NodeId, welcome_ids,
    /// timestamp)`, forwarded by the home to the K homes. Reply:
    /// [`SRelayPacket::WelcomesAcked`] or
    /// [`SRelayPacket::DhtUnavailable`].
    AckWelcomes {
        welcome_ids: Vec<Bytes<8>>,
        timestamp:   u64,
        sig:         Bytes<64>,
    },
}

/// Server Relay Packet
///
/// Packets sent from Server to Client
///
/// SERVER --> CLIENT
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SRelayPacket {
    QueryResult(QueryResultP),
    DispatchAck(DispatchAckP),
    Deliver(DeliverP),

    /// Relay → recipient: an ephemeral signal from a contact (presence/typing).
    Ephemeral(EphemeralP),
    // /// All the pending deliveries for user in chronological order
    // /// TODO: might need debouncing in future if TOO MANY messages were queued at once
    // QueueDrain(Vec<DeliverP>),
    /// Sticky-home — relay-side request for the user to sign a
    /// `QueueFetchAck` transcript over the union of dispatch ids the
    /// recipient relay just drained from the K home relays.
    ///
    /// The relay sends this **after** the client's `AckDrain` arrives
    /// (i.e. once the client has durably stored the delivered set).
    /// Libcore signs
    /// [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`] over
    /// `(self_ipk, requester_relay_id, delivered_ids, timestamp)` and
    /// replies with [`CRelayPacket::AckAuth`]. The relay then fans
    /// out a `QueueFetchAck` to each of the K homes with the same
    /// signed `(timestamp, sig)` pair — the transcript binds the
    /// requesting relay so a captured ack can't be redirected to a
    /// different home via a different relay (cross-relay replay
    /// defense), but it does NOT bind a target-home identity, so one
    /// signature still works across the K homes within R_r's set.
    ///
    /// `requester_relay_id` is this relay's `BLAKE3(NodeKey)` identity
    /// — the same id the relay would supply on `peer/1` `DhtHello`s.
    /// Libcore signs the field's value verbatim into the transcript;
    /// the home cross-checks `requester_relay_id ==
    /// authenticated_peer_id` at the handler layer.
    ///
    /// `suggested_timestamp` is the relay's wall-clock at the moment
    /// of the request; libcore is free to substitute its own
    /// `systime()` (drift is allowed within ±60s) but echoing the
    /// suggested value avoids forcing libcore to query its clock.
    ///
    /// `delivered_ids` is bounded by
    /// [`crate::proto::dht_p2p::MAX_FETCH_QUEUE_ACK_IDS`] (= 64) — the
    /// same ceiling the home-side verifier enforces on
    /// [`crate::proto::dht_p2p::QueueFetchAck::delivered_ids`].
    AckAuthRequest {
        requester_relay_id:  crate::quic::id::NodeId,
        delivered_ids:       Vec<[u8; 16]>,
        suggested_timestamp: u64,
    },

    /// Reply to [`CRelayPacket::PublishKeyPackage`].
    /// `homes_succeeded` is the count of K=3 DHT homes that returned a
    /// success outcome (Stored for Publish, Appended for Refill).
    /// `quorum_met` ⇔ `homes_succeeded ≥ K_MIN` (= 2).
    KeyPackagePublished {
        homes_succeeded: u8,
        quorum_met:      bool,
    },

    /// Reply to [`CRelayPacket::FetchKeyPackage`].
    /// `record = None` collapses the Tier-2 `NoStash` and `NotOwner`
    /// outcomes (libcore can't act on the distinction). `static_hash`
    /// is the cross-replica hash from `KeyPackageFetchFound`
    /// (zeros if `record = None`).
    KeyPackageFetched {
        record:      Option<crate::proto::mls_wire::KeyPackageRecord>,
        remaining:   u32,
        static_hash: Bytes<32>,
    },

    /// Reply to [`CRelayPacket::PublishWelcome`].
    /// `quorum_met = true` ⇔ ≥ K_MIN of the recipient's K=3 homes
    /// stored the envelope.
    WelcomePublished {
        quorum_met: bool,
    },

    /// Reply to [`CRelayPacket::FetchWelcomes`]. The home merges
    /// welcomes from the K=3 home replicas, deduplicates by
    /// `(group_id, kp_ref_used)`, and returns the union.
    WelcomesFetched {
        entries: Vec<crate::proto::mls_wire::WelcomeEntry>,
    },

    /// Reply to [`CRelayPacket::AckWelcomes`].
    WelcomesAcked,

    /// Generic "your home's DHT is disabled" reply, returned for any of
    /// the five wrapper RPCs when `relay.dht.is_none()`. Libcore surfaces
    /// this as a clean per-RPC error rather than retrying — operator must
    /// enable DHT on the home (`relay/config.toml [dht] enabled = true`)
    /// for MLS to function.
    DhtUnavailable,
}

#[cfg(feature = "client")]
impl Sender for CRelayPacket {}

#[cfg(feature = "server")]
impl Sender for SRelayPacket {}

// // // // // // // // // // // // // // // // // //

#[cfg(test)]
mod tests {
    //! Wire-format round-trip + transcript-stability tests for the
    //! sticky-home [`CRelayPacket::DrainAuth`] variant.
    //!
    //! At the wire level this is just a postcard round-trip plus a
    //! byte-stability check that the transcript libcore signs is exactly
    //! what `queue_fetch_signing_input` produces, so any drift between
    //! the signing-input helper and the relay's verifier surfaces here.
    use super::Bytes;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;

    /// Postcard round-trip every Tier-1 wrapper request variant plus the
    /// sticky-home auth packets (`DrainAuth`, `AckAuth`). One catch-all
    /// test: a missing serde derive on any of these variants surfaces here.
    #[test]
    fn phase9_wrapper_request_variants_round_trip() {
        use super::CRelayPacket;
        use crate::proto::mls_wire::KeyPackageRecord;
        use crate::proto::mls_wire::KpPublishMode;
        use crate::proto::mls_wire::WelcomeEnvelopeP;
        use crate::types::bytes::ByteVec;

        let kp_record = KeyPackageRecord {
            ipk:           Bytes([0x11; 32]),
            kp_ref:        ByteVec(vec![0x22; 32]),
            kp_bytes:      ByteVec(vec![0x33; 16]),
            expires_at_ms: 1_700_000_000_000,
            owner_sig:     Bytes([0x44; 64]),
        };
        let envelope = WelcomeEnvelopeP {
            version:       1,
            group_id:      Bytes([0x55; 32]),
            sender_ipk:    Bytes([0x66; 32]),
            recipient_ipk: Bytes([0x77; 32]),
            welcome_blob:  ByteVec(vec![0x88; 64]),
            kp_ref_used:   Bytes([0x99; 32]),
            sender_sig:    Bytes([0xAA; 64]),
            pairing:       None,
        };

        for pkt in [
            CRelayPacket::PublishKeyPackage {
                records:   vec![kp_record.clone()],
                timestamp: 1_700_000_000_001,
                mode:      KpPublishMode::Publish,
                sig:       Bytes([0xBB; 64]),
            },
            CRelayPacket::FetchKeyPackage {
                target_ipk: Bytes([0xCC; 32]),
                timestamp:  1_700_000_000_002,
                sig:        Bytes([0xDD; 64]),
            },
            CRelayPacket::PublishWelcome {
                envelope:  envelope.clone(),
                timestamp: 1_700_000_000_003,
                sig:       Bytes([0xEE; 64]),
            },
            CRelayPacket::FetchWelcomes {
                timestamp: 1_700_000_000_004,
                sig:       Bytes([0xFF; 64]),
            },
            CRelayPacket::AckWelcomes {
                welcome_ids: vec![Bytes([0x01; 8]), Bytes([0x02; 8])],
                timestamp:   1_700_000_000_005,
                sig:         Bytes([0x10; 64]),
            },
            CRelayPacket::DrainAuth {
                timestamp: 1_700_000_000_001,
                sig:       Bytes([0xAB; 64]),
            },
            CRelayPacket::AckAuth {
                sig:       Bytes([0xCD; 64]),
                timestamp: 1_700_000_000_002,
            },
        ] {
            let bytes = pkt.ser().expect("postcard ser");
            let decoded = CRelayPacket::deser(&bytes).expect("postcard deser");
            assert_eq!(decoded, pkt);
        }
    }

    /// Postcard round-trip every Tier-1 wrapper reply variant + the
    /// shared `DhtUnavailable` error reply + the sticky-home
    /// `AckAuthRequest`.
    #[test]
    fn phase9_wrapper_reply_variants_round_trip() {
        use super::SRelayPacket;
        use crate::proto::mls_wire::KeyPackageRecord;
        use crate::proto::mls_wire::WelcomeEntry;
        use crate::proto::mls_wire::WelcomeEnvelopeP;
        use crate::quic::id::NodeId;
        use crate::types::bytes::ByteVec;

        let kp_record = KeyPackageRecord {
            ipk:           Bytes([0x11; 32]),
            kp_ref:        ByteVec(vec![0x22; 32]),
            kp_bytes:      ByteVec(vec![0x33; 16]),
            expires_at_ms: 1_700_000_000_000,
            owner_sig:     Bytes([0x44; 64]),
        };
        let entry = WelcomeEntry {
            welcome_id: Bytes([0x55; 8]),
            envelope:   WelcomeEnvelopeP {
                version:       1,
                group_id:      Bytes([0x66; 32]),
                sender_ipk:    Bytes([0x77; 32]),
                recipient_ipk: Bytes([0x88; 32]),
                welcome_blob:  ByteVec(vec![0x99; 32]),
                kp_ref_used:   Bytes([0xAA; 32]),
                sender_sig:    Bytes([0xBB; 64]),
                pairing:       None,
            },
        };

        for pkt in [
            SRelayPacket::KeyPackagePublished { homes_succeeded: 2, quorum_met: true },
            SRelayPacket::KeyPackageFetched {
                record:      Some(kp_record.clone()),
                remaining:   17,
                static_hash: Bytes([0xCC; 32]),
            },
            SRelayPacket::KeyPackageFetched {
                record:      None,
                remaining:   0,
                static_hash: Bytes([0; 32]),
            },
            SRelayPacket::WelcomePublished { quorum_met: true },
            SRelayPacket::WelcomesFetched { entries: vec![entry.clone()] },
            SRelayPacket::WelcomesAcked,
            SRelayPacket::AckAuthRequest {
                requester_relay_id:  NodeId::new([0x42u8; 32]),
                delivered_ids:       vec![[0xAA; 16], [0xBB; 16], [0xCC; 16]],
                suggested_timestamp: 1_700_000_000_003,
            },
            SRelayPacket::DhtUnavailable,
        ] {
            let bytes = pkt.ser().expect("postcard ser");
            let decoded = SRelayPacket::deser(&bytes).expect("postcard deser");
            assert_eq!(decoded, pkt);
        }
    }
}
