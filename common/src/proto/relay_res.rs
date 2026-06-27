//! Relay to Resolver Proto

use std::fmt::Debug;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::proto::RelayId;
use crate::proto::pack::Packer;
use crate::debug;
use crate::sysutils::SystemLoad;
use crate::trace;
use crate::types::bytes::Bytes;

/// Domain separation tag mixed into the [`LifetimeP::RelayHello`] signed
/// transcript. Bumping this value forces a clean wire-format break.
pub const RELAY_HELLO_SIG_DOMAIN: &[u8] = b"promtuz-relay-hello-v1";

/// Domain separation tag mixed into the [`LifetimeP::RelayHeartbeat`] signed
/// transcript. Distinct from [`RELAY_HELLO_SIG_DOMAIN`] so a captured
/// signature for one packet kind cannot be replayed as the other.
pub const RELAY_HEARTBEAT_SIG_DOMAIN: &[u8] = b"promtuz-relay-heartbeat-v1";

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum LifetimeP {
    /// Initial registration message sent by a relay node to a resolver.
    ///
    /// Carries the relay's full Ed25519 identity public key alongside the
    /// derived [`RelayId`] so the resolver can both verify the id-to-key
    /// binding (`BLAKE3(pubkey)`) and check the attached signature.
    ///
    /// `sig` is an Ed25519 signature over:
    /// `RELAY_HELLO_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
    ///   || relay_id (32 bytes) || pubkey (32 bytes) || timestamp (BE u128)`
    RelayHello {
        /// Stable cryptographic ID derived from the node's public key.
        relay_id:  RelayId,
        /// Full Ed25519 identity public key of the relay. Required so the
        /// resolver can recover the verification key from the wire — a hash
        /// is not invertible, so the id alone is insufficient.
        pubkey:    Bytes<32>,
        timestamp: u128,
        /// Ed25519 signature over the transcript described on this enum.
        sig:       Bytes<64>,
        // TODO: I'd rather use bitset
        // pub capabilities: Vec<String>,
    },

    /// Resolver's acknowledgement of a node registration (`NodeHello`).
    ///
    /// Confirms acceptance, conveys heartbeat timing, or explains rejection.
    HelloAck {
        /// Resolver's current unix time (used for clock-drift checking).
        resolver_time: u128,
    },

    /// Periodic heartbeat sent by a node to indicate that it is still alive
    /// and to provide useful runtime metrics to the resolver.
    ///
    /// Authenticated identically to [`LifetimeP::RelayHello`]: carries the
    /// relay's full Ed25519 pubkey alongside the derived [`RelayId`] so
    /// the resolver can re-verify both id binding and signature on every
    /// heartbeat. Without this any peer that knew a registered relay's
    /// `relay_id` could spoof liveness signals once liveness logic lands.
    ///
    /// `sig` is an Ed25519 signature over:
    /// `RELAY_HEARTBEAT_SIG_DOMAIN || PROTOCOL_VERSION (BE u16)
    ///   || relay_id (32 bytes) || pubkey (32 bytes) || timestamp (BE u128)`
    RelayHeartbeat {
        /// The node's stable cryptographic ID.
        relay_id: RelayId,

        /// Full Ed25519 identity public key of the relay. Carried for the
        /// same reason as [`LifetimeP::RelayHello::pubkey`] — `relay_id` is
        /// a BLAKE3 hash and isn't invertible, so the resolver needs the
        /// full key to verify the attached signature.
        pubkey: Bytes<32>,

        /// Sender-local unix time in milliseconds. Bound into the signed
        /// transcript so the resolver can reject replays outside an
        /// accepted clock-skew window.
        timestamp: u128,

        /// Ed25519 signature over the transcript described on this enum.
        sig: Bytes<64>,

        /// Packed load value:
        ///
        /// upper 7 bits = CPU usage (0–100), lower 7 bits = memory usage (0–100).
        load: SystemLoad,

        /// Node uptime in seconds since its last restart.
        uptime_seconds: u64,
    },
}

/// Builds the canonical signing transcript for [`LifetimeP::RelayHello`].
///
/// Both the relay (signing side) and the resolver (verifying side) call
/// this to derive the exact byte string fed to Ed25519 — using a single
/// helper keeps the two sides byte-for-byte identical.
pub fn relay_hello_signing_input(
    relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128,
) -> Vec<u8> {
    signing_input(RELAY_HELLO_SIG_DOMAIN, relay_id, pubkey, timestamp)
}

/// Builds the canonical signing transcript for [`LifetimeP::RelayHeartbeat`].
///
/// Mirrors [`relay_hello_signing_input`] field-for-field; the only
/// difference is the domain separation tag, which prevents cross-protocol
/// signature replay between the two packet kinds.
pub fn relay_heartbeat_signing_input(
    relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128,
) -> Vec<u8> {
    signing_input(RELAY_HEARTBEAT_SIG_DOMAIN, relay_id, pubkey, timestamp)
}

/// Shared low-level transcript builder. Kept private so callers go through
/// the per-packet helpers above and can't accidentally pass the wrong
/// domain tag.
fn signing_input(
    domain: &[u8], relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(domain.len() + 2 + RelayId::LEN + 32 + 16);
    buf.extend_from_slice(domain);
    buf.extend_from_slice(&crate::PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(relay_id.as_bytes());
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ResolverPacket {
    Lifetime(LifetimeP),
}

impl ResolverPacket {
    pub async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> anyhow::Result<()> {
        let packet = self.pack()?;

        debug!("sent packet ({}B)", packet.len());
        trace!("sent packet {}", hex::encode(&packet));

        tx.write_all(&packet).await?;
        Ok(tx.flush().await?)
    }
}
