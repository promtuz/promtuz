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
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
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
}

#[cfg(feature = "client")]
impl Sender for CRelayPacket {}

#[cfg(feature = "server")]
impl Sender for SRelayPacket {}

// // // // // // // // // // // // // // // // // //
