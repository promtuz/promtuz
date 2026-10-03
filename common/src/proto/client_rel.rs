use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::PROTOCOL_VERSION;
use crate::proto::Sender;
use crate::types::bytes::ByteVec;
use crate::types::bytes::Bytes;

/// Signing domain; every domain string in the protocol is distinct.
pub const DISPATCH_SIG_DOMAIN: &[u8] = b"promtuz-dispatch-v1";

/// Keying material exported under this label ties a client's auth proof to its connection.
pub const CLIENT_AUTH_EXPORTER_LABEL: &[u8] = b"promtuz client auth v1";

/// `binding` is exported under [`CLIENT_AUTH_EXPORTER_LABEL`], so a relay that forwards another
/// relay's challenge gets a proof that verifies only on the client's own connection.
pub fn client_auth_message(nonce: &[u8; 32], binding: &[u8; 32]) -> Vec<u8> {
    [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), nonce, binding].concat()
}

pub fn dispatch_sig_message(
    version: u16, to: &[u8; 32], from: &[u8; 32], id: &[u8; 16], payload: &[u8],
) -> Vec<u8> {
    [DISPATCH_SIG_DOMAIN, &version.to_be_bytes(), to, from, id, payload].concat()
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ServerHandshakeResultP {
    Accept {
        timestamp:     u64,
        /// This relay's DHT NodeId, or `None` with DHT disabled. The phone binds it as
        /// `requester_relay_id` when signing welcome fetches and acks.
        relay_node_id: Option<Bytes<32>>,
        /// Whether this relay answers STUN echoes and bridges TURN on its QUIC port. A bridge
        /// aimed at a relay without it is a silent black hole.
        assist:        bool,
        /// UDP port of this relay's call TURN server, on the host the client dialed.
        turn_port:     Option<u16>,
    },
    Reject {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum CHandshakePacket {
    Hello { ipk: Bytes<32> },
    Proof { sig: Bytes<64> },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SHandshakePacket {
    Challenge { nonce: Bytes<32> },
    HandshakeResult(ServerHandshakeResultP),
}

impl Sender for CHandshakePacket {}

impl Sender for SHandshakePacket {}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum QueryP {
    PubAddress,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum QueryResultP {
    PubAddress { addr: SocketAddr },
    NotFound,
    Error { reason: String },
}

/// `sig` covers [`dispatch_sig_message`]. The relay checks `from` against the session identity,
/// and `id` is minted and signed by the sender, never by the relay.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DispatchP {
    pub to:             Bytes<32>,
    pub from:           Bytes<32>,
    /// The recipient dedups on it, so a retry re-sends the same id.
    pub id:             Bytes<16>,
    pub payload:        ByteVec,
    pub sig:            Bytes<64>,
    /// Stamped by the first relay that accepts the dispatch (clients send zero); outside `sig`.
    pub accepted_at_ms: u64,
    /// Plaintext hint outside `sig`: whether and how urgently to wake an offline recipient.
    pub wake: Wake,
    /// Plaintext hint outside `sig`: milliseconds past `accepted_at_ms` the dispatch stays worth
    /// delivering. Zero means the relay's default retention.
    pub ttl_ms: u64,
}

impl DispatchP {
    /// Past its sender-declared life at `now_ms`, so a queue drops it unsent. Zero never expires
    /// here; the store's retention sweep still bounds it. A call offer never expires here: its
    /// late delivery is how the recipient records the missed call.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.wake != Wake::Call
            && self.ttl_ms != 0
            && now_ms.saturating_sub(self.accepted_at_ms) > self.ttl_ms
    }
}

/// Ordinals are wire format: `No` and `Message` encode exactly as the bool `false` and `true`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Queued silently: receipts, edits, reactions, pair acks.
    No,
    /// New content the recipient should see soon.
    Message,
    /// A call offer: the platform's highest push priority, and it rings only until it expires.
    Call,
}

impl Wake {
    pub fn wakes(self) -> bool {
        self != Wake::No
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct TurnCredentialsP {
    pub username:      String,
    pub password:      String,
    /// The relay's clock; allocations refuse the credentials past it.
    pub expires_at_ms: u64,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct DeliverP {
    /// Copied from [`DispatchP::id`]; the recipient dedups on `(from, id)`.
    pub id:             Bytes<16>,
    pub from:           Bytes<32>,
    pub payload:        ByteVec,
    pub sig:            Bytes<64>,
    /// Origin relay acceptance time, copied unchanged through queues and DHT
    /// forwarding. Recipients use it rather than local receive time.
    pub accepted_at_ms: u64,
    /// Copied from [`DispatchP::ttl_ms`]; zero means default retention.
    pub ttl_ms:         u64,
}

/// Bits OR'd into [`ActivityP::activity`]; zero is a bare presence heartbeat.
pub const ACTIVITY_TYPING: u16 = 1 << 0;
/// Can be used for both emoji and sticker
pub const ACTIVITY_CHOOSING_STICKER: u16 = 1 << 1;
pub const ACTIVITY_UPLOADING_MEDIA: u16 = 1 << 2;
pub const ACTIVITY_UPLOADING_DOCUMENT: u16 = 1 << 3;
pub const ACTIVITY_RECORDING_VN: u16 = 1 << 4;

pub const ACTIVITY_SIG_DOMAIN: &[u8] = b"promtuz-activity-v1";

/// Cleartext and relay-routed, not MLS: delivered only to an online recipient, never queued.
/// `sig` under `from` lets the recipient and relay reject a forged signal.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct ActivityP {
    pub to:           Bytes<32>,
    pub from:         Bytes<32>,
    /// The shared MLS group, never a device-local conversation id; signed so a relay cannot move
    /// activity between chats.
    pub group_id:     Bytes<32>,
    pub activity:     u16,
    pub timestamp:    u64,
    pub sig:          Bytes<64>,
}

pub fn activity_sig_message(
    to: &[u8; 32], from: &[u8; 32], group_id: &[u8; 32], activity: u16, timestamp: u64,
) -> Vec<u8> {
    [
        ACTIVITY_SIG_DOMAIN,
        &PROTOCOL_VERSION.to_be_bytes(),
        to,
        from,
        group_id,
        &activity.to_be_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

/// Replaces the caller's whole interest set; every contact needs a granted, signed consent.
/// Presence flows only between mutual subscribers.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SubscribePresenceP {
    pub contacts: Vec<Bytes<32>>,
    pub consents: Vec<crate::proto::dht_p2p::PresenceConsent>,
    pub lease:    crate::proto::dht_p2p::PresenceLease,
}

/// `Online` needs the client's `SetPresence(Active)`; `Idle` is asserted on backgrounding. A
/// frozen app can hold its connection until the idle timeout, so connected is not online.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum PresenceState {
    Online,
    /// Connected but backgrounded since this unix-ms.
    Idle { since: u64 },
    /// `last_seen` is unix ms; `0` = unknown (e.g. relay restart).
    Offline { last_seen: u64 },
}

/// Relay-asserted, not peer-signed: the relay already sees connection metadata.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct PresenceP {
    pub who:   Bytes<32>,
    pub state: PresenceState,
}

/// `Idle` on backgrounding, `Active` on returning to the foreground.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum PresenceMode {
    Active,
    Idle,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum DispatchAckP {
    Queued {
        accepted_at_ms: u64,
    },
    Delivered {
        accepted_at_ms: u64,
    },
    NotFound,
    InvalidSig,
    /// The recipient's queue is full and the message was not stored; back off.
    QueueFull,
    /// Queued at the required number of the recipient's DHT homes, unlike the local-only
    /// [`Self::Queued`].
    Forwarded {
        accepted_at_ms: u64,
    },
    Error {
        reason: String,
    },
}

/// Variant ordinals are wire format: append only.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum CRelayPacket {
    Query(QueryP),
    Dispatch(DispatchP),

    Activity(ActivityP),

    DeliverAck,

    DrainQueue,
    /// The drained messages the client stored durably. The relay clears only these, so the rest
    /// come again on the next drain.
    AckDrain { ids: Vec<[u8; 16]> },

    /// The user's [`crate::proto::dht_p2p::queue_fetch_signing_input`] signature, which this
    /// relay presents as `QueueFetch.user_sig` to every home in the user's set.
    DrainAuth {
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// Reply to [`SRelayPacket::AckAuthRequest`]: the user's signature over
    /// [`crate::proto::dht_p2p::queue_fetch_ack_signing_input`], so a relay cannot forge deletion.
    AckAuth {
        sig:       Bytes<64>,
        timestamp: u64,
    },

    /// User-signed: `sig` covers the inner publish transcript, and the home checks it, then
    /// forwards it unchanged to the storage homes, which check it again.
    PublishKeyPackage {
        records:   Vec<crate::proto::mls_wire::KeyPackageRecord>,
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// Gate-only: the home checks `sig` over
    /// [`crate::proto::mls_wire::kp_fetch_wrap_signing_input`] and does not forward it.
    FetchKeyPackage {
        target_ipk: Bytes<32>,
        timestamp:  u64,
        sig:        Bytes<64>,
    },

    /// Gate-only: the user's authority rides in `envelope.sender_sig`; `sig` covers
    /// [`crate::proto::mls_wire::welcome_publish_wrap_signing_input`] and stays at the home.
    PublishWelcome {
        envelope:  crate::proto::mls_wire::WelcomeEnvelopeP,
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// User-signed over [`crate::proto::mls_wire::welcome_fetch_signing_input`] with this home's
    /// NodeId as `requester_relay_id`; the home forwards the same signature to the storage homes.
    FetchWelcomes {
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// User-signed over [`crate::proto::mls_wire::welcome_ack_signing_input`]; forwarded like
    /// [`Self::FetchWelcomes`], and the storage homes delete the listed welcomes.
    AckWelcomes {
        welcome_ids: Vec<Bytes<8>>,
        timestamp:   u64,
        sig:         Bytes<64>,
    },

    /// No reply on this stream; presence arrives as [`SRelayPacket::Presence`] on the relay's own
    /// streams.
    SubscribePresence(SubscribePresenceP),

    SetPresence(PresenceMode),

    /// Our push pseudonym `P`, so this home can wake our offline queue without the device token.
    /// `sig` covers [`crate::proto::dht_p2p::push_pseudonym_signing_input`]. No reply.
    RegisterPush {
        pseudonym: Bytes<32>,
        timestamp: u64,
        sig:       Bytes<64>,
    },

    /// Minted for the connection-authenticated IPK.
    TurnCredentials,

    /// Service guarantees on this connection. Silence from an old relay proves nothing.
    ServiceCapabilities,
    /// Bounded owner-signed services::key_inventory::Request.
    KeyPackageInventory { request: ByteVec },
}

/// Variant ordinals are wire format: append only.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SRelayPacket {
    QueryResult(QueryResultP),
    DispatchAck(DispatchAckP),
    Deliver(DeliverP),

    Activity(ActivityP),
    /// Sent after `AckDrain`: the client signs `queue_fetch_ack_signing_input` over these ids and
    /// replies with [`CRelayPacket::AckAuth`]; it may use its own clock for the timestamp.
    AckAuthRequest {
        requester_relay_id:  crate::types::id::NodeId,
        delivered_ids:       Vec<[u8; 16]>,
        suggested_timestamp: u64,
    },

    /// `quorum_met` needs one success with a sole selected home, otherwise two.
    KeyPackagePublished {
        homes_succeeded: u8,
        quorum_met:      bool,
    },

    /// `None` covers both `NoStash` and `NotOwner`; `static_hash` is then zero.
    KeyPackageFetched {
        record:      Option<crate::proto::mls_wire::KeyPackageRecord>,
        remaining:   u32,
        static_hash: Bytes<32>,
    },

    /// `quorum_met` needs one stored copy with a sole selected home, otherwise two.
    WelcomePublished {
        quorum_met: bool,
    },

    /// The union from the storage homes, deduplicated by `(group_id, kp_ref_used)`.
    WelcomesFetched {
        entries: Vec<crate::proto::mls_wire::WelcomeEntry>,
    },

    WelcomesAcked,

    /// The home runs without DHT; any of the five MLS wrapper RPCs can get this.
    DhtUnavailable,

    /// A snapshot right after `SubscribePresence`, then single-entry deltas.
    Presence(Vec<PresenceP>),

    /// `None` when this relay runs no TURN server.
    TurnCredentials(Option<TurnCredentialsP>),

    /// Canonical bounded contracts::Support in the service namespace.
    ServiceCapabilities { supported: ByteVec },
    /// Bounded services::key_inventory::Inventory, including unavailable homes.
    KeyPackageInventory { inventory: ByteVec },
}

impl Sender for CRelayPacket {}

impl Sender for SRelayPacket {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts() {
        crate::proto::golden(
            &[
                client_auth_message(&[1; 32], &[2; 32]),
                dispatch_sig_message(PROTOCOL_VERSION, &[1; 32], &[2; 32], &[3; 16], b"payload"),
                activity_sig_message(&[1; 32], &[2; 32], &[3; 32], 0x0102, 0x0304),
            ],
            "b784a6c7ee42b34603c07eddccd6eda1f0830755ccad8b9fe8dad5f1453862f8
             3db888a1b396d726a8ab4186fc1fc441ffa66ab6b6524a635aa479b8aa76418e
             363343bbafd645880d89d826eb2eea80ecdcbca5de46c4a1202efed33c066bd7",
        );
    }

    #[test]
    fn dispatches_signed_under_an_accepted_version_verify() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let from = key.verifying_key().to_bytes();
        let transcript = |v| dispatch_sig_message(v, &[2; 32], &from, &[3; 16], b"payload");
        let verifies = |signed_at: u16| {
            let sig = key.sign(&transcript(signed_at)).to_bytes();
            crate::crypto::verify_versioned(&from, &sig, transcript).is_ok()
        };
        for (version, accepted) in [(PROTOCOL_VERSION, true), (10, true), (9, false), (12, false)] {
            assert_eq!(verifies(version), accepted, "signed under version {version}");
        }
    }
}
