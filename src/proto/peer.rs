//! TODO
//! 
//! Contains Proto for any Peers

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