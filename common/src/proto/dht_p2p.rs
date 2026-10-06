//! DHT relay-to-relay wire protocol: one `DhtRequest` and one `DhtResponse` per bi-stream.
//! Transcripts are `domain || PROTOCOL_VERSION (BE u16) || fields`, and every domain is distinct.

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::proto::RelayId;
use crate::proto::client_rel::ActivityP;
use crate::proto::client_rel::PresenceState;
use crate::proto::pack::bounded_vec;
use crate::types::bytes::{Bytes,ByteVec};

pub const DHT_HELLO_SIG_DOMAIN: &[u8] = b"promtuz-dht-hello-v1";

/// Also bounds the timestamps of `Forward`, `QueueFetch(Ack)` and other signed DHT RPCs.
pub const MAX_DHT_HELLO_SKEW_MS: u64 = 60_000;

/// Replication factor `k`.
pub const DHT_K: usize = 3;

pub const MAX_FIND_NODE_RESULTS: usize = DHT_K;

/// First frame on a `peer/N` connection; binds the dialer's `NodeId` to the connection.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DhtHello {
    pub node_id:   crate::types::id::NodeId,
    pub pubkey:    Bytes<32>,
    pub timestamp: u64,
    pub sig:       Bytes<64>,
}

/// `binding` ties the hello to this TLS session.
pub fn dht_hello_signing_input(
    node_id: &crate::types::id::NodeId, pubkey: &[u8; 32], timestamp: u64, binding: &[u8; 32],
) -> Vec<u8> {
    [
        DHT_HELLO_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        node_id.as_bytes(),
        pubkey,
        &timestamp.to_be_bytes(),
        binding,
    ]
    .concat()
}

/// The label a [`DhtHello`]'s session binding is exported under.
pub const DHT_HELLO_EXPORTER_LABEL: &[u8] = b"promtuz dht hello v1";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DhtHelloVerifyError {
    #[error("dht hello: node_id != BLAKE3(pubkey)")]
    IdMismatch,
    #[error("dht hello: malformed Ed25519 pubkey")]
    MalformedPubkey,
    #[error("dht hello: bad signature")]
    BadSignature,
    #[error("dht hello: stale or future timestamp (clock skew)")]
    ClockSkew,
}

#[cfg(feature = "crypto")]
mod verify_impl {
    use ed25519_dalek::Signature;
    use ed25519_dalek::VerifyingKey;

    use super::DhtHello;
    use super::DhtHelloVerifyError;
    use super::Forward;
    use super::ForwardVerifyError;
    use super::MAX_DHT_HELLO_SKEW_MS;
    use super::MAX_FETCH_QUEUE_ACK_IDS;
    use super::PRESENCE_LEASE_MAX_MS;
    use super::PRESENCE_STATE_MAX_SKEW_MS;
    use super::PresenceConsent;
    use super::PresenceLease;
    use super::QueueFetch;
    use super::QueueFetchAck;
    use super::QueueFetchAckVerifyError;
    use super::QueueFetchVerifyError;
    use super::RelayPresenceState;
    use super::dht_hello_signing_input;
    use super::forward_signing_input;
    use super::presence_consent_signing_input;
    use super::presence_lease_signing_input;
    use super::presence_state_signing_input;
    use super::queue_fetch_ack_signing_input;
    use super::queue_fetch_signing_input;
    use crate::crypto::verify_ed25519;
    use crate::types::id::NodeId;

    impl DhtHello {
        pub fn verify(&self, now_ms: u64, binding: &[u8; 32]) -> Result<(), DhtHelloVerifyError> {
            let derived_id = NodeId::new(self.pubkey.as_ref());
            if derived_id != self.node_id {
                return Err(DhtHelloVerifyError::IdMismatch);
            }

            let vk = VerifyingKey::from_bytes(&self.pubkey.0)
                .map_err(|_| DhtHelloVerifyError::MalformedPubkey)?;

            let sig = Signature::from_bytes(&self.sig.0);
            let msg = dht_hello_signing_input(&self.node_id, &self.pubkey.0, self.timestamp, binding);
            vk.verify_strict(&msg, &sig).map_err(|_| DhtHelloVerifyError::BadSignature)?;

            let skew = now_ms.abs_diff(self.timestamp);
            if skew > MAX_DHT_HELLO_SKEW_MS {
                return Err(DhtHelloVerifyError::ClockSkew);
            }

            Ok(())
        }
    }

    impl PresenceConsent {
        pub fn verify(&self, now_ms: u64) -> bool {
            if now_ms.abs_diff(self.issued_at_ms) > PRESENCE_STATE_MAX_SKEW_MS {
                return false;
            }
            let msg = presence_consent_signing_input(
                &self.owner.0,
                &self.recipient.0,
                self.version,
                self.issued_at_ms,
                self.granted,
            );
            verify_ed25519(&self.owner.0, &msg, &self.user_sig.0).is_ok()
        }
    }

    impl PresenceLease {
        pub fn verify(&self, now_ms: u64) -> bool {
            if self.expires_at_ms <= self.issued_at_ms
                || self.expires_at_ms - self.issued_at_ms > PRESENCE_LEASE_MAX_MS
                || now_ms > self.expires_at_ms
                || self.issued_at_ms > now_ms + PRESENCE_STATE_MAX_SKEW_MS
            {
                return false;
            }
            let msg = presence_lease_signing_input(
                &self.user.0,
                &self.relay_id,
                self.version,
                self.issued_at_ms,
                self.expires_at_ms,
            );
            verify_ed25519(&self.user.0, &msg, &self.user_sig.0).is_ok()
        }
    }

    impl RelayPresenceState {
        pub fn verify(&self, authenticated_relay: &NodeId, now_ms: u64) -> bool {
            self.who == self.lease.user
                && self.lease.relay_id == *authenticated_relay
                && NodeId::new(self.relay_pubkey.0) == self.lease.relay_id
                && self.lease.verify(now_ms)
                && now_ms.abs_diff(self.observed_at_ms) <= PRESENCE_STATE_MAX_SKEW_MS
                && verify_ed25519(
                    &self.relay_pubkey.0,
                    &presence_state_signing_input(self),
                    &self.relay_sig.0,
                )
                .is_ok()
        }
    }

    impl Forward {
        /// Checks the outer relay signature and timestamp only; the home checks `dispatch.sig` at
        /// delivery. `sender_relay_pubkey` comes from the connection's verified `DhtHello`.
        pub fn verify(
            &self, sender_relay_pubkey: &[u8; 32], now_ms: u64,
        ) -> Result<(), ForwardVerifyError> {
            let vk = VerifyingKey::from_bytes(sender_relay_pubkey)
                .map_err(|_| ForwardVerifyError::MalformedField)?;

            let sig = Signature::from_bytes(&self.sig.0);
            let msg =
                forward_signing_input(&self.dispatch.id.0, &self.sender_relay_id, self.timestamp);
            vk.verify_strict(&msg, &sig).map_err(|_| ForwardVerifyError::BadForwardSig)?;

            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(ForwardVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(ForwardVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }

    impl QueueFetch {
        pub fn verify(&self, now_ms: u64) -> Result<(), QueueFetchVerifyError> {
            let vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| QueueFetchVerifyError::MalformedField)?;

            let sig = Signature::from_bytes(&self.user_sig.0);
            let msg = queue_fetch_signing_input(
                &self.user_ipk.0,
                &self.requester_relay_id,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig).map_err(|_| QueueFetchVerifyError::BadUserSig)?;

            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }

    impl QueueFetchAck {
        /// Bounds `delivered_ids` before any crypto. The handler, not this, matches
        /// `requester_relay_id` to the connection's authenticated peer.
        pub fn verify(&self, now_ms: u64) -> Result<(), QueueFetchAckVerifyError> {
            if self.delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS {
                return Err(QueueFetchAckVerifyError::TooManyIds);
            }

            let vk = VerifyingKey::from_bytes(&self.user_ipk.0)
                .map_err(|_| QueueFetchAckVerifyError::MalformedField)?;

            let sig = Signature::from_bytes(&self.user_sig.0);
            let msg = queue_fetch_ack_signing_input(
                &self.user_ipk.0,
                &self.requester_relay_id,
                &self.delivered_ids,
                self.timestamp,
            );
            vk.verify_strict(&msg, &sig).map_err(|_| QueueFetchAckVerifyError::BadUserSig)?;

            if now_ms > self.timestamp && now_ms - self.timestamp > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchAckVerifyError::StaleTimestamp);
            }
            if self.timestamp > now_ms && self.timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
                return Err(QueueFetchAckVerifyError::FutureTimestamp);
            }

            Ok(())
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDescriptor {
    pub id:     RelayId,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr:   SocketAddr,
    pub pubkey: Bytes<32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindNode {
    pub target:    Bytes<32>,
    pub requester: RelayId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindNodeResp {
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_FIND_NODE_RESULTS>")]
    pub closer: Vec<NodeDescriptor>,
}

pub const DHT_FORWARD_SIG_DOMAIN: &[u8] = b"promtuz-dht-forward-v1";

/// The replica holds no platform token; only the gateway maps a pseudonym to one.
pub const DHT_PUSH_PSEUDONYM_SIG_DOMAIN: &[u8] = b"promtuz-dht-push-pseudonym-v1";
pub const DHT_LIVE_FORWARD_SIG_DOMAIN: &[u8] = b"promtuz-dht-live-forward-v1";

pub fn push_pseudonym_signing_input(
    user_ipk: &[u8; 32], pseudonym: &[u8; 32], timestamp: u64,
) -> Vec<u8> {
    [
        DHT_PUSH_PSEUDONYM_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        user_ipk,
        pseudonym,
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPseudonymPublish {
    pub user_ipk:  Bytes<32>,
    pub pseudonym: Bytes<32>,
    pub timestamp: u64,
    pub user_sig:  Bytes<64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPseudonymPublishResp {
    pub accepted: bool,
}

/// `dispatch.sig` is the sender's end-to-end signature; `sig` is the forwarding relay's, for
/// attribution and replay defence.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub dispatch:        crate::proto::client_rel::DispatchP,
    pub sender_relay_id: crate::types::id::NodeId,
    pub timestamp:       u64,
    pub sig:             Bytes<64>,
}

/// Only the dispatch id: `dispatch.sig` already covers the payload.
pub fn forward_signing_input(
    dispatch_id: &[u8; 16], sender_relay_id: &crate::types::id::NodeId, timestamp: u64,
) -> Vec<u8> {
    [
        DHT_FORWARD_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        dispatch_id,
        sender_relay_id.as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardOutcome {
    Delivered,
    Stored,
    NotOwner,
    QueueFull,
    /// Either the embedded `dispatch.sig` or the outer `sig` failed.
    BadSig,
    RateLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardResp {
    pub outcome: ForwardOutcome,
}

/// Delivered only if the recipient is connected to this home; never stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityForward {
    pub activity: ActivityP,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityForwardResp {
    pub delivered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceConsent {
    pub owner:        Bytes<32>,
    pub recipient:    Bytes<32>,
    pub version:      u64,
    pub issued_at_ms: u64,
    pub granted:      bool,
    pub user_sig:     Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceLease {
    pub user:          Bytes<32>,
    pub relay_id:      crate::types::id::NodeId,
    pub version:       u64,
    pub issued_at_ms:  u64,
    pub expires_at_ms: u64,
    pub user_sig:      Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPresenceState {
    pub recipient:      Bytes<32>,
    pub who:            Bytes<32>,
    pub lease:          PresenceLease,
    pub state:          PresenceState,
    pub version:        u64,
    pub observed_at_ms: u64,
    pub relay_pubkey:   Bytes<32>,
    pub relay_sig:      Bytes<64>,
}

pub const PRESENCE_CONSENT_SIG_DOMAIN: &[u8] = b"promtuz-presence-consent-v1";
pub const PRESENCE_LEASE_SIG_DOMAIN: &[u8] = b"promtuz-presence-lease-v1";
pub const PRESENCE_STATE_SIG_DOMAIN: &[u8] = b"promtuz-presence-state-v1";
pub const PRESENCE_LEASE_MAX_MS: u64 = 10 * 60 * 1000;
pub const PRESENCE_STATE_MAX_SKEW_MS: u64 = 60_000;

pub fn presence_consent_signing_input(
    owner: &[u8; 32], recipient: &[u8; 32], version: u64, issued_at_ms: u64, granted: bool,
) -> Vec<u8> {
    [
        PRESENCE_CONSENT_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        owner,
        recipient,
        &version.to_be_bytes(),
        &issued_at_ms.to_be_bytes(),
        &[granted as u8],
    ]
    .concat()
}

pub fn presence_lease_signing_input(
    user: &[u8; 32], relay_id: &crate::types::id::NodeId, version: u64, issued_at_ms: u64,
    expires_at_ms: u64,
) -> Vec<u8> {
    [
        PRESENCE_LEASE_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        user,
        relay_id.as_bytes(),
        &version.to_be_bytes(),
        &issued_at_ms.to_be_bytes(),
        &expires_at_ms.to_be_bytes(),
    ]
    .concat()
}

pub fn presence_state_signing_input(record: &RelayPresenceState) -> Vec<u8> {
    let (state, state_ts) = match record.state {
        PresenceState::Online => (0u8, 0),
        PresenceState::Idle { since } => (1, since),
        PresenceState::Offline { last_seen } => (2, last_seen),
    };
    [
        PRESENCE_STATE_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        &record.recipient.0,
        &record.who.0,
        &record.lease.user.0,
        record.lease.relay_id.as_bytes(),
        &record.lease.version.to_be_bytes(),
        &record.lease.issued_at_ms.to_be_bytes(),
        &record.lease.expires_at_ms.to_be_bytes(),
        &record.lease.user_sig.0,
        &[state],
        &state_ts.to_be_bytes(),
        &record.version.to_be_bytes(),
        &record.observed_at_ms.to_be_bytes(),
        &record.relay_pubkey.0,
    ]
    .concat()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceReplicationResp {
    pub accepted: bool,
}

/// Recipient homes retain this user-signed assignment long enough to attempt
/// live delivery at the relay currently serving the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveForward {
    pub dispatch:        crate::proto::client_rel::DispatchP,
    pub lease:           PresenceLease,
    pub sender_relay_id: crate::types::id::NodeId,
    pub timestamp:       u64,
    pub sig:             Bytes<64>,
}

pub fn live_forward_signing_input(
    dispatch_id: &[u8; 16], lease: &PresenceLease, sender_relay_id: &crate::types::id::NodeId,
    timestamp: u64,
) -> Vec<u8> {
    [
        DHT_LIVE_FORWARD_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        dispatch_id,
        &lease.user.0,
        &lease.version.to_be_bytes(),
        lease.relay_id.as_bytes(),
        sender_relay_id.as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveForwardResp {
    pub delivered: bool,
}

pub const DHT_QUEUE_FETCH_SIG_DOMAIN: &[u8] = b"promtuz-dht-queue-fetch-v1";

/// Signed by the user, not the requesting relay: only the user can authorise draining their queue.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetch {
    pub user_ipk:           Bytes<32>,
    /// Signed, so a captured `user_sig` cannot be redirected to another requester.
    pub requester_relay_id: crate::types::id::NodeId,
    pub timestamp:          u64,
    pub user_sig:           Bytes<64>,
}

pub fn queue_fetch_signing_input(
    user_ipk: &[u8; 32], requester_relay_id: &crate::types::id::NodeId, timestamp: u64,
) -> Vec<u8> {
    [
        DHT_QUEUE_FETCH_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        user_ipk,
        requester_relay_id.as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

pub const MAX_FETCH_QUEUE_BATCH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchResp {
    /// Oldest first.
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_FETCH_QUEUE_BATCH>")]
    pub messages:  Vec<crate::proto::client_rel::DispatchP>,
    pub exhausted: bool,
}

pub const DHT_QUEUE_FETCH_ACK_SIG_DOMAIN: &[u8] = b"promtuz-dht-queue-fetch-ack-v1";

/// Signed by the user so a relay cannot forge deletion of a queue, and bound to the requesting
/// relay so a captured ack cannot be redirected to another home.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchAck {
    pub user_ipk:           Bytes<32>,
    pub requester_relay_id: crate::types::id::NodeId,
    pub delivered_ids:      Vec<[u8; 16]>,
    pub timestamp:          u64,
    pub user_sig:           Bytes<64>,
}

pub fn queue_fetch_ack_signing_input(
    user_ipk: &[u8; 32], requester_relay_id: &crate::types::id::NodeId, delivered_ids: &[[u8; 16]],
    timestamp: u64,
) -> Vec<u8> {
    [
        DHT_QUEUE_FETCH_ACK_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        user_ipk,
        requester_relay_id.as_bytes(),
        &(delivered_ids.len() as u32).to_be_bytes(),
        delivered_ids.as_flattened(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

/// One ack covers exactly one fetch batch.
pub const MAX_FETCH_QUEUE_ACK_IDS: usize = MAX_FETCH_QUEUE_BATCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFetchAckResp {
    pub ok: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ForwardVerifyError {
    /// Never returned by [`Forward::verify`]; the home checks the embedded signature at delivery.
    #[error("forward: bad embedded dispatch signature")]
    BadDispatchSig,
    #[error("forward: bad outer sender-relay signature")]
    BadForwardSig,
    #[error("forward: stale timestamp (clock skew)")]
    StaleTimestamp,
    #[error("forward: future timestamp (clock skew)")]
    FutureTimestamp,
    #[error("forward: malformed sender-relay pubkey")]
    MalformedField,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueFetchVerifyError {
    #[error("queue fetch: bad user signature")]
    BadUserSig,
    #[error("queue fetch: stale timestamp (clock skew)")]
    StaleTimestamp,
    #[error("queue fetch: future timestamp (clock skew)")]
    FutureTimestamp,
    #[error("queue fetch: malformed user_ipk")]
    MalformedField,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueFetchAckVerifyError {
    #[error("queue fetch ack: bad user signature")]
    BadUserSig,
    #[error("queue fetch ack: stale timestamp (clock skew)")]
    StaleTimestamp,
    #[error("queue fetch ack: future timestamp (clock skew)")]
    FutureTimestamp,
    #[error("queue fetch ack: malformed user_ipk")]
    MalformedField,
    #[error("queue fetch ack: delivered_ids exceeds MAX_FETCH_QUEUE_ACK_IDS")]
    TooManyIds,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtRequest {
    FindNode(FindNode),
    Forward(Forward),
    ActivityForward(ActivityForward),
    PresenceConsent(PresenceConsent),
    PresenceState(RelayPresenceState),
    PresenceLease(PresenceLease),
    LiveForward(LiveForward),
    PushPseudonymPublish(PushPseudonymPublish),
    QueueFetch(QueueFetch),
    QueueFetchAck(QueueFetchAck),

    KeyPackagePublish(crate::proto::mls_wire::KeyPackagePublishReq),
    KeyPackageFetch(crate::proto::mls_wire::KeyPackageFetchReq),

    WelcomePublish(crate::proto::mls_wire::WelcomePublishReq),
    WelcomeFetch(crate::proto::mls_wire::WelcomeFetchReq),
    WelcomeAck(crate::proto::mls_wire::WelcomeAckReq),
    /// Connection-bound storage-service guarantees. Appended for ordinal stability.
    ServiceCapabilities,
    /// Owner authorization bound to the authenticated forwarding relay.
    KeyPackageInventory { request: ByteVec },
}

/// Pairs 1:1 with [`DhtRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtResponse {
    FindNode(FindNodeResp),
    Forward(ForwardResp),
    ActivityForward(ActivityForwardResp),
    PresenceConsent(PresenceReplicationResp),
    PresenceState(PresenceReplicationResp),
    PresenceLease(PresenceReplicationResp),
    LiveForward(LiveForwardResp),
    PushPseudonymPublish(PushPseudonymPublishResp),
    QueueFetch(QueueFetchResp),
    QueueFetchAck(QueueFetchAckResp),

    KeyPackagePublish(crate::proto::mls_wire::KeyPackagePublishResp),
    KeyPackageFetch(crate::proto::mls_wire::KeyPackageFetchResp),

    WelcomePublish(crate::proto::mls_wire::WelcomePublishResp),
    WelcomeFetch(crate::proto::mls_wire::WelcomeFetchResp),
    WelcomeAck(crate::proto::mls_wire::WelcomeAckResp),
    ServiceCapabilities { supported: ByteVec },
    /// One bounded home snapshot, encoded as services::key_inventory::Inventory.
    KeyPackageInventory { inventory: ByteVec },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DhtPacket {
    Request(Box<DhtRequest>),
    Response(Box<DhtResponse>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::id::NodeId;

    #[test]
    fn transcripts() {
        let relay = NodeId::from_bytes([9; 32]);
        let lease = PresenceLease {
            user:          Bytes([1; 32]),
            relay_id:      relay,
            version:       0x0102,
            issued_at_ms:  0x0304,
            expires_at_ms: 0x0506,
            user_sig:      Bytes([2; 64]),
        };
        let state = |state| {
            presence_state_signing_input(&RelayPresenceState {
                recipient: Bytes([3; 32]),
                who: Bytes([1; 32]),
                lease: lease.clone(),
                state,
                version: 0x0708,
                observed_at_ms: 0x090a,
                relay_pubkey: Bytes([4; 32]),
                relay_sig: Bytes([5; 64]),
            })
        };
        crate::proto::golden(
            &[
                dht_hello_signing_input(&relay, &[1; 32], 0x0102, &[2; 32]),
                push_pseudonym_signing_input(&[1; 32], &[2; 32], 0x0102),
                forward_signing_input(&[1; 16], &relay, 0x0102),
                presence_consent_signing_input(&[1; 32], &[2; 32], 0x0102, 0x0304, true),
                presence_lease_signing_input(&[1; 32], &relay, 0x0102, 0x0304, 0x0506),
                state(PresenceState::Online),
                state(PresenceState::Idle { since: 0x0b0c }),
                state(PresenceState::Offline { last_seen: 0x0d0e }),
                live_forward_signing_input(&[6; 16], &lease, &relay, 0x0f10),
                queue_fetch_signing_input(&[1; 32], &relay, 0x0102),
                queue_fetch_ack_signing_input(&[1; 32], &relay, &[[2; 16], [3; 16]], 0x0102),
            ],
            "55722476e11ed826ca585dce9070676953f79858c3365d47e6d5a17d4d79a0d4
             e9c847d67652e43ae5f49d69d4d27874ea49d630f0fcb2cd759e1cee12311aa0
             f3ba4410100a76adf3076cbe824c8f1560aa7c4b6b9154f9d844d189b88d062b
             0e2261eae89f81ffa6c2d52ad6d867d691489281a560c983708160886c800b5a
             866211ece7aaef3ec839487062c4c81116a6980d1cfcb1903de161d8b6b24c79
             70f3f8cdfa33932902adb21b8e124f4e83fe7366dc33c78b5fc89ad674ece2ab
             6c623594ed54c9d556e7eca753117b577baeb68e8d02b7ae5fe7be04fdcd0ec8
             d1aab7f715fec04ad9025ae70f0ba4b1dd431dbb45b18be49f30de554e3fd8ab
             1f84cf1610ed6d2f29b45d66aa2175f196eae67809f83a75fb28078b0e6e1c9b
             eda7c915e8fe7329325d81f10ddde6fbda93565be917874ff1afdd5aac17d727
             b4819d29afe3baaad7f3db70d090faf3cf77a714deaa4d26d596ce182b2045a8",
        );
    }
}
