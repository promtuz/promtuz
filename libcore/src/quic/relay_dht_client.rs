//! [`DhtClient`] over the home relay connection. The home verifies each wrapper and runs the K-home
//! fan-out for us, forwarding our user signature verbatim for the storage homes to re-verify.

use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::MAX_FRAMED_MLS_BYTES;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::kp_fetch_wrap_signing_input;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use quinn::Connection;

use crate::data::identity::IdentitySigner;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
use crate::quic::dht_client::DhtClientResult;

/// K-quorum minimum the home enforces (`FORWARD_K_MIN`); mirrored here
/// only to populate the `QuorumNotMet` error detail.
const HOME_K_MIN: usize = 2;

pub struct RelayDhtClient {
    conn: Connection,
    /// Our IPK, bound by every wrapper transcript.
    user_ipk: [u8; 32],
    /// `None` when the home has DHT disabled; welcome fetch and ack then fail fast.
    home_node_id: Option<[u8; 32]>,
}

impl RelayDhtClient {
    pub fn new(conn: Connection, user_ipk: [u8; 32], home_node_id: Option<[u8; 32]>) -> Self {
        Self {
            conn,
            user_ipk,
            home_node_id,
        }
    }

    fn sign(&self, msg: &[u8]) -> DhtClientResult<[u8; 64]> {
        IdentitySigner::sign(msg)
            .map(|s| s.to_bytes())
            .map_err(|e| DhtClientError::Protocol(format!("sign: {e}")))
    }

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

    async fn publish_inner(&self, records: &[KeyPackageRecord]) -> DhtClientResult<()> {
        let timestamp = now_ms();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
        let count = records.len() as u32;
        let msg = kp_publish_signing_input(MLS_WIRE_VERSION, &self.user_ipk, &digest, count, timestamp);
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::PublishKeyPackage {
            records: records.to_vec(),
            timestamp,
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
    async fn publish_keypackages(&self, records: &[KeyPackageRecord]) -> DhtClientResult<()> {
        self.publish_inner(records).await
    }

    async fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> DhtClientResult<KeyPackageRecord> {
        let timestamp = now_ms();
        let msg = kp_fetch_wrap_signing_input(MLS_WIRE_VERSION, &self.user_ipk, target_ipk, timestamp);
        let sig = self.sign(&msg)?;
        let req = CRelayPacket::FetchKeyPackage {
            target_ipk: Bytes(*target_ipk),
            timestamp,
            sig: Bytes(sig),
        };
        match self.rpc(req).await? {
            SRelayPacket::KeyPackageFetched { record: Some(record), .. } => Ok(record),
            SRelayPacket::KeyPackageFetched { record: None, .. } => Err(DhtClientError::NoStash),
            SRelayPacket::DhtUnavailable => Err(dht_unavailable()),
            other => Err(unexpected(&other)),
        }
    }

    async fn deliver_welcome(&self, envelope: &WelcomeEnvelopeP) -> DhtClientResult<()> {
        let payload = MlsEnvelopeP::Welcome(envelope.clone())
            .ser()
            .map_err(|e| DhtClientError::Protocol(format!("ser welcome envelope: {e}")))?;

        // DispatchP rides a u16 frame; a 1:1 welcome fits, a group one wouldn't.
        if payload.len() > MAX_FRAMED_MLS_BYTES {
            return Err(DhtClientError::Protocol(format!(
                "welcome payload {} exceeds MAX_FRAMED_MLS_BYTES {MAX_FRAMED_MLS_BYTES}",
                payload.len(),
            )));
        }

        let to = envelope.recipient_ipk.0;
        let id = random_dispatch_id();
        let sig_message = dispatch_sig_message(common::PROTOCOL_VERSION, &to, &self.user_ipk, &id, &payload);
        let sig = self.sign(&sig_message)?;
        let fwd = DispatchP {
            to:      Bytes(to),
            from:    Bytes(self.user_ipk),
            id:      Bytes(id),
            payload: ByteVec(payload),
            sig:     Bytes(sig),
            accepted_at_ms: 0,
            // First-contact welcome: the peer must be woken to receive it.
            wake:    common::proto::client_rel::Wake::Message,
            ttl_ms:  0,
        };

        match self.rpc(CRelayPacket::Dispatch(fwd)).await? {
            // Delivered (live) / Forwarded / Queued (offline) = relay owns it.
            SRelayPacket::DispatchAck(
                DispatchAckP::Delivered { .. }
                | DispatchAckP::Forwarded { .. }
                | DispatchAckP::Queued { .. },
            ) => Ok(()),
            // Not stored → caller rolls back the group.
            SRelayPacket::DispatchAck(other) => Err(DhtClientError::Protocol(format!(
                "relay rejected welcome dispatch: {other:?}"
            ))),
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

/// Welcomes have no persisted dispatch id; the recipient dedups on this one.
fn random_dispatch_id() -> [u8; 16] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    id
}

fn dht_unavailable() -> DhtClientError {
    DhtClientError::Transport("home relay has DHT disabled".into())
}

fn unexpected(reply: &SRelayPacket) -> DhtClientError {
    DhtClientError::Protocol(format!("unexpected reply: {reply:?}"))
}
