//! Client to Relay Proto

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::proto::Sender;
use crate::types::bytes::ByteVec;
use crate::types::bytes::Bytes;

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
/// sig covers: "relay-forward-v1" || to || from || payload
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct ForwardP {
    pub to:      Bytes<32>,
    pub from:    Bytes<32>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
}

/// Relay → Client (relay-verified delivery)
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DeliverP {
    pub from:    Bytes<32>,
    pub payload: ByteVec,
    pub sig:     Bytes<64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ForwardResultP {
    Accepted,
    NotFound,
    InvalidSig,
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
    Forward(ForwardP),
}

/// Server Relay Packet
///
/// Packets sent from Server to Client
///
/// SERVER --> CLIENT
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SRelayPacket {
    QueryResult(QueryResultP),
    ForwardResult(ForwardResultP),
    Deliver(DeliverP),
}

#[cfg(feature = "client")]
impl Sender for CRelayPacket {}

#[cfg(feature = "server")]
impl Sender for SRelayPacket {}

// // // // // // // // // // // // // // // // // //
