//! Production [`DhtClient`] over the home relay's `client/0` connection
//! (§3.9 — the Option-B dialer that replaces `Peer1DhtClient`).
//!
//! libcore no longer dials `peer/1` or holds an ephemeral DhtHello
//! identity. Every DHT operation is a `CRelayPacket` wrapper sent over
//! the *existing* authenticated relay connection; the home relay
//! verifies the wrapper, originates the real K-home fan-out on our
//! behalf, and replies with the matching `SRelayPacket`. The phone has
//! exactly one upstream — its home — and is invisible at the
//! relay-to-relay protocol layer.
//!
//! For the three user-signed RPCs (KP publish/refill, welcome
//! fetch/ack) the signature we attach *is* the inner Tier-2 user
//! signature the K storage homes re-verify; the home forwards it
//! verbatim. Welcome fetch/ack bind the home's NodeId (learned from the
//! handshake, [`crate::data::relay::Relay::home_node_id`]) as
//! `requester_relay_id`.

use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::SRelayPacket;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KpPublishMode;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::kp_fetch_wrap_signing_input;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_refill_signing_input;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::mls_wire::welcome_publish_wrap_signing_input;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use quinn::Connection;

use crate::data::identity::IdentitySigner;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
use crate::quic::dht_client::DhtClientResult;
use crate::quic::dht_client::FetchedKeyPackage;
use crate::quic::dht_client::KpOutcomeFilter;
use crate::quic::dht_client::PublishOutcome;

/// K-quorum minimum the home enforces (`FORWARD_K_MIN`); mirrored here
/// only to populate the `QuorumNotMet` error detail.
const HOME_K_MIN: usize = 2;

/// [`DhtClient`] backed by the live home relay connection.
pub struct RelayDhtClient {
    /// The authenticated `client/0` connection to our home relay.
    conn: Connection,
    /// Our own IPK — every wrapper transcript binds it.
    user_ipk: [u8; 32],
    /// The home's DHT NodeId, learned from the handshake. `None` when
    /// the home has DHT disabled; welcome fetch/ack then fail fast with
    /// a `DhtUnavailable`-flavoured error (matching the reply the home
    /// would send anyway).
    home_node_id: Option<[u8; 32]>,
    /// Test seam (feature `e2e-client`): when `Some`, DHT-RPC transcripts
    /// are signed with this explicit key instead of the global, JNI-backed
    /// [`IdentitySigner`], so the headless e2e harness can run off-device
    /// (the Android `KeyManager` is unavailable there). Production never
    /// sets it; the key MUST correspond to `user_ipk`.
    #[cfg(feature = "e2e-client")]
    explicit_signer: Option<ed25519_dalek::SigningKey>,
}

impl RelayDhtClient {
    pub fn new(conn: Connection, user_ipk: [u8; 32], home_node_id: Option<[u8; 32]>) -> Self {
        Self {
            conn,
            user_ipk,
            home_node_id,
            #[cfg(feature = "e2e-client")]
            explicit_signer: None,
        }
    }

    /// Headless/e2e constructor (feature `e2e-client`): sign DHT-RPC with
    /// `signer` — whose verifying key MUST equal `user_ipk` — instead of
    /// the global keystore, so the harness runs without the Android
    /// `KeyManager`. Restores the explicit-signer capability the deleted
    /// `Peer1DhtClient` had.
    #[cfg(feature = "e2e-client")]
    pub(crate) fn new_with_signer(
        conn: Connection,
        user_ipk: [u8; 32],
        home_node_id: Option<[u8; 32]>,
        signer: ed25519_dalek::SigningKey,
    ) -> Self {
        Self { conn, user_ipk, home_node_id, explicit_signer: Some(signer) }
    }

    /// Sign `msg` under our long-term IPK.
    fn sign(&self, msg: &[u8]) -> DhtClientResult<[u8; 64]> {
        #[cfg(feature = "e2e-client")]
        if let Some(sk) = &self.explicit_signer {
            use ed25519_dalek::Signer as _;
            return Ok(sk.sign(msg).to_bytes());
        }
        IdentitySigner::sign(msg)
            .map(|s| s.to_bytes())
            .map_err(|e| DhtClientError::Protocol(format!("sign: {e}")))
    }

    /// One request → one reply over a fresh bi-stream on the home
    /// connection. Mirrors the dispatch round-trip in `messaging.rs`.
    async fn rpc(&self, req: CRelayPacket) -> DhtClientResult<SRelayPacket> {
        let (mut tx, mut rx) = self
            .conn
            .open_bi()
            .await
            .map_err(|e| DhtClientError::Transport(e.to_string()))?;
        req.send(&mut tx).await.map_err(|e| DhtClientError::Transport(e.to_string()))?;
        let _ = tx.finish();
        SRelayPacket::unpack(&mut rx).await.map_err(|e| DhtClientError::Transport(e.to_string()))
    }

    async fn publish_inner(
        &self, records: &[KeyPackageRecord], mode: KpPublishMode,
    ) -> DhtClientResult<()> {
        let timestamp = now_ms();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
        let count = records.len() as u32;
        let msg = match mode {
            KpPublishMode::Publish => {
                kp_publish_signing_input(MLS_WIRE_VERSION, &self.user_ipk, &digest, count, timestamp)
            },
            KpPublishMode::Refill => {
                kp_refill_signing_input(MLS_WIRE_VERSION, &self.user_ipk, &digest, count, timestamp)
            },
        };
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::PublishKeyPackage {
            records: records.to_vec(),
            timestamp,
            mode,
            sig: Bytes(sig),
        };
        match self.rpc(req).await? {
            SRelayPacket::KeyPackagePublished { homes_succeeded, quorum_met } => {
                if quorum_met {
                    Ok(())
                } else {
                    Err(DhtClientError::QuorumNotMet {
                        succeeded: homes_succeeded as usize,
                        wanted: HOME_K_MIN,
                    })
                }
            },
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }
}

impl DhtClient for RelayDhtClient {
    async fn publish_keypackages(
        &self, records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        self.publish_inner(records, KpPublishMode::Publish).await
    }

    async fn refill_keypackages(
        &self, records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        self.publish_inner(records, KpPublishMode::Refill).await
    }

    async fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> DhtClientResult<FetchedKeyPackage> {
        let timestamp = now_ms();
        let msg = kp_fetch_wrap_signing_input(MLS_WIRE_VERSION, &self.user_ipk, target_ipk, timestamp);
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::FetchKeyPackage {
            target_ipk: Bytes(*target_ipk),
            timestamp,
            sig: Bytes(sig),
        };
        match self.rpc(req).await? {
            SRelayPacket::KeyPackageFetched { record, remaining, static_hash } => match record {
                Some(record) => Ok(FetchedKeyPackage { record, remaining, static_hash: static_hash.0 }),
                None => Err(DhtClientError::NoStash),
            },
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }

    async fn publish_welcome_to_homes(
        &self, envelope: &WelcomeEnvelopeP,
    ) -> DhtClientResult<PublishOutcome> {
        let timestamp = now_ms();
        let msg = welcome_publish_wrap_signing_input(
            MLS_WIRE_VERSION, &self.user_ipk, &envelope.welcome_blob.0, timestamp,
        );
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::PublishWelcome {
            envelope: envelope.clone(),
            timestamp,
            sig: Bytes(sig),
        };
        match self.rpc(req).await? {
            // The send path (`messaging.rs`) rolls back the founder's
            // group state on `Err`, so a missed quorum must surface as
            // an error, not `Ok(Failed)`.
            SRelayPacket::WelcomePublished { quorum_met } => {
                if quorum_met {
                    Ok(PublishOutcome::Stored)
                } else {
                    Err(DhtClientError::QuorumNotMet { succeeded: 0, wanted: HOME_K_MIN })
                }
            },
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }

    async fn fetch_welcomes(&self) -> DhtClientResult<Vec<WelcomeEntry>> {
        let node_id = self.require_home_node_id()?;
        let timestamp = now_ms();
        let msg = welcome_fetch_signing_input(MLS_WIRE_VERSION, &self.user_ipk, &node_id, timestamp);
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::FetchWelcomes { timestamp, sig: Bytes(sig) };
        match self.rpc(req).await? {
            SRelayPacket::WelcomesFetched { entries } => Ok(entries),
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }

    async fn ack_welcomes(&self, welcome_ids: &[[u8; 8]]) -> DhtClientResult<()> {
        let node_id = self.require_home_node_id()?;
        let timestamp = now_ms();
        let msg =
            welcome_ack_signing_input(MLS_WIRE_VERSION, &self.user_ipk, &node_id, welcome_ids, timestamp);
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::AckWelcomes {
            welcome_ids: welcome_ids.iter().map(|id| Bytes(*id)).collect(),
            timestamp,
            sig: Bytes(sig),
        };
        match self.rpc(req).await? {
            SRelayPacket::WelcomesAcked => Ok(()),
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }
}

impl RelayDhtClient {
    fn require_home_node_id(&self) -> DhtClientResult<NodeId> {
        self.home_node_id
            .map(NodeId::from_bytes)
            .ok_or_else(dht_unavailable)
    }
}

fn now_ms() -> u64 {
    crate::utils::systime().as_millis() as u64
}

fn dht_unavailable() -> DhtClientError {
    DhtClientError::Transport("home relay has DHT disabled".into())
}

fn unexpected(reply: &SRelayPacket) -> DhtClientError {
    DhtClientError::Protocol(format!("unexpected reply: {reply:?}"))
}
