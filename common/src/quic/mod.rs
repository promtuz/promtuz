use anyhow::Result;
use quinn::Connection;
use quinn::VarInt;

pub mod config;
pub mod id;
#[cfg(feature = "server")]
pub mod p256;
pub mod protorole;
pub mod xor;

pub use xor::xor32;

/// Heartbeat interval in seconds
pub static RESOLVER_RELAY_HEARTBEAT_INTERVAL: u64 = 20;

pub async fn send_uni(conn: &Connection, data: &[u8]) -> Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(data).await?;
    send.finish()?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CloseReason {
    DuplicateConnect,
    AlreadyConnected,
    ShuttingDown,
    Reconnecting,
    PacketMismatch,
    /// Resolver: incoming `RelayHello` failed signature/identity validation
    /// (id-to-key mismatch, malformed pubkey, or bad Ed25519 sig).
    BadSignature,
    /// Resolver: `RelayHello.timestamp` is outside the accepted clock window.
    StaleTimestamp,
    /// Resolver: registry is at capacity, no more relays can be admitted
    /// until existing ones disconnect.
    RegistryFull,
    /// Peer ALPN-negotiated a protocol role (e.g. `resolver/1`) for which
    /// this side has no implementation. Closing politely is preferable to
    /// panicking the spawned per-connection task.
    UnsupportedRole,
    /// Source address has exceeded its accept-side rate-limit quota.
    /// Returned at the acceptor before any per-connection state is created.
    RateLimited,
    /// DHT (`peer/1`): a record's `user_sig` or `relay_sig` failed to
    /// verify. Per `misc/specs/DHT.md` §2.5.
    DhtBadSignature,
    /// DHT (`peer/1`): a record's `not_before` is more than
    /// `PRESENCE_MAX_FUTURE_SKEW_MS` in the future, or `not_after` has
    /// already elapsed at the time of receipt. Per §2.5.
    DhtClockSkew,
    /// DHT (`peer/1`): peer asked us to STORE a record outside our
    /// k-closest ownership window and we declined. Per §2.5 / §5.4.
    DhtNotOwner,
    /// DHT (`peer/1`): per-peer rate limit on `Store`/`FetchRecord`
    /// tripped. Per §2.5 / §8.4.
    DhtFlood,
    /// DHT (`peer/1`): a wire field violated its declared length bound
    /// (see `dht_p2p`'s `MAX_*` consts). Per §2.5 / §2.6.
    DhtMalformedKey,
    /// DHT (`peer/1`): sticky-home `Forward` / `QueueFetch` /
    /// `QueueFetchAck` RPC was rejected for a hard protocol violation
    /// the wire-format validator surfaced (e.g. bad outer signature on
    /// `Forward`, ack-id list overflow on `QueueFetchAck`). The
    /// soft-reject outcomes (`QueueFull`, `NotOwner`, `RateLimited`)
    /// are returned in the response body and do **not** close the
    /// connection. Per `misc/specs/STICKY_HOME_RELAY.md` §5.3.
    DhtForwardRejected,
    /// MLS — `KeyPackagePublish` / `KeyPackageRefill` /
    /// `KeyPackageFetch` RPC failed because some piece of the
    /// payload was structurally malformed: the publisher's outer
    /// `sig` did not verify, a per-record `owner_sig` did not verify,
    /// the embedded openmls `KeyPackage` rejected validation, the
    /// batch exceeded `KP_STASH_TARGET`, or a static-fields conflict
    /// was detected (§13.3). Maps from
    /// [`crate::proto::mls_wire::KeyPackagePublishOutcome::BadSig`] /
    /// `TooMany` / `StaticFieldsConflict` and the analogous Refill
    /// variants. Per `misc/specs/MLS.md` §3.4 / §3.6 / §13.3.
    KeyPackageMalformed,
    /// MLS — record's `expires_at_ms` had already elapsed at
    /// store time, or the publisher's `timestamp` is outside the
    /// ±`MAX_KP_SKEW_MS` skew window. Distinct from
    /// [`Self::KeyPackageMalformed`] so operators can attribute
    /// clock-drift problems separately from forged-signature problems.
    /// Per `misc/specs/MLS.md` §3.4 (Expired outcome).
    KeyPackageExpired,
    /// MLS — per-`(target_ipk, requester_relay_id)` rate
    /// limit on KeyPackage fetches tripped (`MAX_KP_FETCH_PER_HOUR
    /// = 60`, §0). Distinct from [`Self::DhtFlood`] (which is the
    /// general per-peer per-RPC-class limiter) because the KP fetch
    /// limiter is keyed on the (target, requester) *pair*, not on
    /// the requester alone — so a peer hammering a single target
    /// trips this code, while a peer hammering many targets at the
    /// per-peer rate trips `DhtFlood`. Per `misc/specs/MLS.md` §5.6.
    KeyPackageRateLimited,
    /// MLS — `WelcomePublish` / `WelcomeFetch` / `WelcomeAck`
    /// rejected for a hard protocol violation: bad envelope sig, bad
    /// user-fetch sig, requester binding mismatch, oversize blob,
    /// recipient_ipk mismatch in the embedded welcome envelope, or
    /// any other structural malformation. Distinct from
    /// [`Self::KeyPackageMalformed`] so operators can attribute
    /// welcome-flow failures separately from KP-flow failures.
    /// Per the welcome-queue spec.
    WelcomeMalformed,
    /// MLS — `WelcomePublish` was rejected because the
    /// recipient's welcome queue is at
    /// [`crate::proto::mls_wire::MAX_WELCOMES_PER_RECIPIENT`]. Soft
    /// outcome; surfaces in the response body, not on the close
    /// channel — this variant exists so the *forwarding* relay can
    /// optionally treat repeated `QueueFull`s as a "stop trying this
    /// home" signal in a future hardening pass.
    WelcomeQueueFull,
    /// MLS — per-relay rate limit on welcome RPCs tripped.
    /// Distinct from [`Self::DhtFlood`] for the same reason
    /// [`Self::KeyPackageRateLimited`] is.
    WelcomeRateLimited,
}

impl CloseReason {
    pub fn reason(&self) -> Vec<u8> {
        format!("{:?}", self).into()
    }
    pub fn code(&self) -> VarInt {
        VarInt::from_u32(*self as u32 + 1)
    }

    pub fn close(self, conn: &Connection) {
        conn.close(self.code(), &self.reason());
    }
}
