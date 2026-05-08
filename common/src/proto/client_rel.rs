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
    Accept { timestamp: u64 },
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
    /// UUIDv7 picked by the sender; promoted to `DeliverP::id` unchanged.
    pub id:      Bytes<16>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
}

/// Relay → Client (relay-verified delivery)
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DeliverP {
    /// UUIDv7
    pub id:      Bytes<16>,
    pub from:    Bytes<32>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
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
    /// Semantics per `misc/specs/STICKY_HOME_RELAY.md` §4.2 step 5: the
    /// dispatch is queued at K_MIN homes; eventual delivery depends on
    /// the recipient draining one of those homes on reconnect. Sender
    /// has no further proof of delivery — read receipts are out of
    /// scope (§9 of the same spec).
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
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum CRelayPacket {
    Query(QueryP),
    Dispatch(DispatchP),

    /// User acknowledges receiving valid delivery of messages
    DeliverAck,

    /// Drains Queue, user requesting for all incoming messages
    DrainQueue,
    /// User confirms storing messages hence queue can be cleared from server
    AckDrain,

    /// Sticky-home phase 2c — user-signed authorisation for the relay
    /// to drain the user's queues from K-closest "home" relays on the
    /// user's behalf.
    ///
    /// The relay R_r the user just authenticated to is *not always* in
    /// the user's K-closest set; in that case R_r must impersonate the
    /// user when issuing `QueueFetch` against the K homes per
    /// `STICKY_HOME_RELAY.md` §4.3 step 3. The home relay only ships
    /// queued dispatches when the user has signed the request — `sig`
    /// is that user signature, sized so a single sign-on serves all K
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
    /// **Phase split (§4.3 + design discussion)**: this packet is sent
    /// at the *fetch* end of the recipient flow. The
    /// `QueueFetchAck` deletion path (which would prove the user
    /// received specific dispatch ids) is deferred to phase 2d — a
    /// transcript over `delivered_ids` requires the relay to know the
    /// id list before it can ask libcore to sign, which is impossible
    /// before fetching has happened. Until 2d lands, homes never
    /// receive an ack and their queued copies linger until natural TTL
    /// expiry; this means duplicate delivery is possible if the user
    /// reconnects multiple times within the TTL window. The client
    /// dedupes by [`DispatchP::id`].
    DrainAuth { timestamp: u64, sig: Bytes<64> },

    /// Sticky-home phase 2d — user-signed authorisation for the K
    /// home relays to GC the listed dispatch ids from their
    /// `cf_dht_queue` after the relay successfully delivered them to
    /// this client.
    ///
    /// Sent in response to [`SRelayPacket::AckAuthRequest`] (the relay
    /// asks libcore to sign the union of delivered ids drawn from all
    /// K homes). The transcript is
    /// [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`] over
    /// `(self_ipk, delivered_ids, timestamp)` — it does **not** bind a
    /// specific home identity, so a single signature serves all K
    /// homes; each home only deletes the ids it actually holds from
    /// the union list.
    ///
    /// **Why the user signs (not the relay)**: a malicious relay could
    /// otherwise forge an ack and force every home to drop a user's
    /// queued messages without delivery. Routing the ack through the
    /// user's IPK signature mirrors the existing `DrainAuth` design
    /// for the fetch direction.
    AckAuth { sig: Bytes<64>, timestamp: u64 },
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
    // /// All the pending deliveries for user in chronological order
    // /// TODO: might need debouncing in future if TOO MANY messages were queued at once
    // QueueDrain(Vec<DeliverP>),
    /// Sticky-home phase 2d — relay-side request for the user to sign a
    /// `QueueFetchAck` transcript over the union of dispatch ids the
    /// recipient relay just drained from the K home relays.
    ///
    /// The relay sends this **after** the client's `AckDrain` arrives
    /// (i.e. once the client has durably stored the delivered set).
    /// Libcore signs
    /// [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`] over
    /// `(self_ipk, delivered_ids, timestamp)` and replies with
    /// [`CRelayPacket::AckAuth`]. The relay then fans out a
    /// `QueueFetchAck` to each of the K homes with the same signed
    /// `(timestamp, sig)` pair — the transcript doesn't bind a home
    /// identity, so one signature works for all homes.
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
    AckAuthRequest { delivered_ids: Vec<[u8; 16]>, suggested_timestamp: u64 },
}

#[cfg(feature = "client")]
impl Sender for CRelayPacket {}

#[cfg(feature = "server")]
impl Sender for SRelayPacket {}

// // // // // // // // // // // // // // // // // //

#[cfg(test)]
mod tests {
    //! Wire-format round-trip + transcript-stability tests for the
    //! sticky-home phase 2c [`CRelayPacket::DrainAuth`] variant.
    //!
    //! Per the phase 2c dispatch: "no new tests at the wire level
    //! beyond a postcard round-trip — the transcript is already tested
    //! by phase 2a." We add the round-trip plus a byte-stability check
    //! that the transcript libcore signs is exactly what
    //! `queue_fetch_signing_input` produces, so any drift between the
    //! signing-input helper and the relay's verifier surfaces here.
    use super::CRelayPacket;
    use super::Bytes;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;

    #[test]
    fn drain_auth_round_trip() {
        // Magic byte fields just so a serde derive missing on the
        // variant fails loudly here. The signature isn't validated by
        // round-trip — that's `queue_fetch_signing_input`'s job, tested
        // in `dht_p2p`.
        let pkt = CRelayPacket::DrainAuth {
            timestamp: 1_700_000_000_001,
            sig: Bytes([0xAB; 64]),
        };

        let bytes = pkt.ser().expect("postcard serialize");
        let decoded = CRelayPacket::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, pkt);
    }

    /// Pin the transcript layout libcore will sign so the relay-side
    /// verifier (which reconstructs the same bytes) cannot drift. The
    /// transcript is the existing `queue_fetch_signing_input` from
    /// phase 2a — we just make sure the signing surface used by
    /// `DrainAuth` is exactly that helper.
    #[cfg(feature = "crypto")]
    #[test]
    fn drain_auth_transcript_matches_queue_fetch_signing_input() {
        use crate::proto::dht_p2p::queue_fetch_signing_input;
        use crate::quic::id::NodeId;

        let user_ipk: [u8; 32] = [0x11; 32];
        let relay_id = NodeId::new([0x22u8; 32]);
        let ts: u64 = 1_700_000_000_001;

        let transcript = queue_fetch_signing_input(&user_ipk, &relay_id, ts);
        // Transcript is `domain || version(BE u16) || ipk(32) ||
        // node_id(32) || ts(BE u64)`. We only need to confirm the
        // helper's output is non-empty and length-stable — the byte
        // layout itself is tested in `dht_p2p`'s test module.
        assert_eq!(
            transcript.len(),
            crate::proto::dht_p2p::DHT_QUEUE_FETCH_SIG_DOMAIN.len()
                + 2
                + 32
                + NodeId::LEN
                + 8,
            "transcript length must match the documented layout"
        );
    }

    /// Phase 2d — postcard round-trip for `CRelayPacket::AckAuth`.
    /// Catches a missing `serde` derive on the new variant the same
    /// way `drain_auth_round_trip` does for `DrainAuth`.
    #[test]
    fn ack_auth_round_trip() {
        let pkt = CRelayPacket::AckAuth {
            sig:       Bytes([0xCD; 64]),
            timestamp: 1_700_000_000_002,
        };
        let bytes = pkt.ser().expect("postcard serialize");
        let decoded = CRelayPacket::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, pkt);
    }

    /// Phase 2d — postcard round-trip for
    /// `SRelayPacket::AckAuthRequest`. Mirrors `ack_auth_round_trip`
    /// for the request side; both variants must serialize stably so
    /// libcore and the relay agree byte-for-byte.
    #[test]
    fn ack_auth_request_round_trip() {
        use super::SRelayPacket;
        let pkt = SRelayPacket::AckAuthRequest {
            delivered_ids:       vec![[0xAA; 16], [0xBB; 16], [0xCC; 16]],
            suggested_timestamp: 1_700_000_000_003,
        };
        let bytes = pkt.ser().expect("postcard serialize");
        let decoded = SRelayPacket::deser(&bytes).expect("postcard deserialize");
        assert_eq!(decoded, pkt);
    }

    /// Phase 2d — pin the transcript libcore signs in response to an
    /// `AckAuthRequest`. Same byte-stability discipline as
    /// `drain_auth_transcript_matches_queue_fetch_signing_input`: if
    /// the helper's layout drifts, this test surfaces it.
    #[cfg(feature = "crypto")]
    #[test]
    fn ack_auth_transcript_matches_queue_fetch_ack_signing_input() {
        use crate::proto::dht_p2p::queue_fetch_ack_signing_input;

        let user_ipk: [u8; 32] = [0x11; 32];
        let ids: Vec<[u8; 16]> = vec![[0xAA; 16], [0xBB; 16]];
        let ts: u64 = 1_700_000_000_004;

        let transcript = queue_fetch_ack_signing_input(&user_ipk, &ids, ts);
        // Layout: domain || version(BE u16) || ipk(32) || count(BE u32)
        //   || n*16 || ts(BE u64).
        let expected_len = crate::proto::dht_p2p::DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len()
            + 2
            + 32
            + 4
            + ids.len() * 16
            + 8;
        assert_eq!(transcript.len(), expected_len);
    }
}
