//! Client-relay hole-punch assist wire: a STUN address echo and a blind TURN datagram bridge.
//! STUN control is plaintext; TURN payloads are the peers' own QUIC, opaque to the relay.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

/// The first byte clears the QUIC fixed bit, and the prefix differs from disco's `.p2p`.
pub const MAGIC: [u8; 4] = [0x2e, 0x70, 0x52, 0x72]; // ".pRr"

const TAG_STUN_REQ: u8 = 1;
const TAG_STUN_RESP: u8 = 2;
const TAG_TURN_ALLOC: u8 = 3;
const TAG_TURN_DATA: u8 = 4;

/// Secret bytes both peers derive from their MLS group; the token names and gates one bridge.
pub const TOKEN_LEN: usize = 16;

const HDR: usize = MAGIC.len() + 1;

#[derive(Debug, PartialEq, Eq)]
pub enum RelayMsg<'a> {
    StunReq { tx: [u8; 8] },
    StunResp { tx: [u8; 8], seen: SocketAddr },
    TurnAlloc { token: [u8; TOKEN_LEN] },
    TurnData { token: [u8; TOKEN_LEN], payload: &'a [u8] },
}

impl RelayMsg<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HDR + 32);
        out.extend_from_slice(&MAGIC);
        match self {
            RelayMsg::StunReq { tx } => {
                out.push(TAG_STUN_REQ);
                out.extend_from_slice(tx);
            },
            RelayMsg::StunResp { tx, seen } => {
                out.push(TAG_STUN_RESP);
                out.extend_from_slice(tx);
                put_addr(&mut out, *seen);
            },
            RelayMsg::TurnAlloc { token } => {
                out.push(TAG_TURN_ALLOC);
                out.extend_from_slice(token);
            },
            RelayMsg::TurnData { token, payload } => {
                out.push(TAG_TURN_DATA);
                out.extend_from_slice(token);
                out.extend_from_slice(payload);
            },
        }
        out
    }

    pub fn decode(pkt: &[u8]) -> Option<RelayMsg<'_>> {
        if pkt.len() < HDR || !pkt.starts_with(&MAGIC) {
            return None;
        }
        let body = &pkt[HDR..];
        match pkt[MAGIC.len()] {
            TAG_STUN_REQ => Some(RelayMsg::StunReq { tx: body.get(..8)?.try_into().ok()? }),
            TAG_STUN_RESP => Some(RelayMsg::StunResp {
                tx:   body.get(..8)?.try_into().ok()?,
                seen: get_addr(body.get(8..)?)?,
            }),
            TAG_TURN_ALLOC => {
                Some(RelayMsg::TurnAlloc { token: body.get(..TOKEN_LEN)?.try_into().ok()? })
            },
            TAG_TURN_DATA => Some(RelayMsg::TurnData {
                token:   body.get(..TOKEN_LEN)?.try_into().ok()?,
                payload: &body[TOKEN_LEN..],
            }),
            _ => None,
        }
    }
}

pub fn is_assist(pkt: &[u8]) -> bool {
    pkt.len() >= HDR && pkt.starts_with(&MAGIC)
}

/// `port(2, be) | family(1) | ip(4 or 16)`.
fn put_addr(out: &mut Vec<u8>, addr: SocketAddr) {
    out.extend_from_slice(&addr.port().to_be_bytes());
    match addr.ip() {
        IpAddr::V4(v4) => {
            out.push(4);
            out.extend_from_slice(&v4.octets());
        },
        IpAddr::V6(v6) => {
            out.push(6);
            out.extend_from_slice(&v6.octets());
        },
    }
}

fn get_addr(b: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes(b.get(..2)?.try_into().ok()?);
    let (fam, ip) = b.get(2..)?.split_first()?;
    match fam {
        4 => {
            let o: [u8; 4] = ip.get(..4)?.try_into().ok()?;
            Some((Ipv4Addr::from(o), port).into())
        },
        6 => {
            let o: [u8; 16] = ip.get(..16)?.try_into().ok()?;
            Some((Ipv6Addr::from(o), port).into())
        },
        _ => None,
    }
}
