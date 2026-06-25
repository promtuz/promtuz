//! Peer connection candidates for **NAT traversal** (STUN/TURN +
//! hole-punching) — unbuilt, planned future work.
//!
//! Today a relay must be reachable at the address the resolver records from
//! `conn.remote_address()`, so a relay behind NAT can dial out but can't be
//! dialed back — cross-relay links only form to publicly-reachable peers.
//! The `ConnectionCandidate` sketch below is the design starting point for
//! lifting that limitation (see also the planned BLE transport).

// use std::net::SocketAddr;

// use anyhow::Result;
// use serde::{Deserialize, Serialize};

// #[derive(Serialize, Deserialize, Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
// enum ConnectionCandidate {
//     Direct(SocketAddr),         // LAN or known public IP
//     StunDiscovered(SocketAddr), // Public IP discovered via STUN
//     TurnRelay(SocketAddr),      // Relay server IP
// }


// impl ConnectionCandidate {
//     async fn collect() -> Result<Vec<Self>> { 
//         Ok(vec![])
//     } 
// }