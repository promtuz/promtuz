//! Direct peer-to-peer transport: punch a NAT hole and stand up a
//! direct QUIC link between two clients, so calls and >256KB transfers
//! skip the store-and-forward relay.
//!
//! The relay stays the fallback (and the signaling path — candidates
//! ride the existing MLS channel), but bulk/live traffic goes straight
//! device-to-device once a hole is open. Built bottom-up: the poke wire
//! (`disco`) and the socket that carries it come first; the punch state
//! machine and session manager sit on top.

// Submodules land as the stack is built; each is exercised by the layer
// above it, so the compiler can't see all users until the top is wired.
#![allow(dead_code)]

mod disco;
mod socket;
