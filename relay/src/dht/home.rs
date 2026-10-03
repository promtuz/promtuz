//! Home side of sticky-home routing: each home delivers a forwarded dispatch live or queues it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use common::crypto::verify_ed25519;
use common::proto::Sender;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::PresenceP;
use common::proto::client_rel::SRelayPacket;
use common::proto::dht_p2p::ActivityForward;
use common::proto::dht_p2p::ActivityForwardResp;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::Forward;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::dht_p2p::ForwardResp;
use common::proto::dht_p2p::LiveForward;
use common::proto::dht_p2p::LiveForwardResp;
use common::proto::dht_p2p::PresenceConsent;
use common::proto::dht_p2p::PresenceLease;
use common::proto::dht_p2p::PresenceReplicationResp;
use common::proto::dht_p2p::RelayPresenceState;
use common::proto::dht_p2p::live_forward_signing_input;
use common::quic::id::NodeId;
use common::quic::xor32;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use tokio::time::timeout;

use super::Dht;
use super::config::FORWARD_LIVE_DELIVERY_MS;
use super::config::FORWARD_TIMEOUT_MS;
use super::forward::activity_is_authentic;
use super::forward::verify_dispatch_user_sig;
use super::rpc::rpc;
use crate::quic::handler::client::events::forward::dispatch_to_deliver;
use crate::quic::handler::client::events::forward::try_deliver;

pub(crate) async fn handle_activity_forward_rpc(
    dht: &Arc<Dht>, forward: ActivityForward, now_ms: u64,
) -> ActivityForwardResp {
    let activity = forward.activity;
    if !activity_is_authentic(&activity, now_ms) {
        return ActivityForwardResp { delivered: false };
    }
    let conn = dht.clients.as_ref().and_then(|clients| clients.read().get(&activity.to.0).cloned());
    let Some(conn) = conn else { return ActivityForwardResp { delivered: false } };
    let delivered = if let Ok((mut tx, _)) = conn.open_bi().await {
        SRelayPacket::Activity(activity).send(&mut tx).await.is_ok() && tx.finish().is_ok()
    } else {
        false
    };
    ActivityForwardResp { delivered }
}

pub(crate) async fn handle_presence_consent_rpc(
    dht: &Arc<Dht>, consent: PresenceConsent, now_ms: u64,
) -> PresenceReplicationResp {
    if !consent.verify(now_ms) {
        return PresenceReplicationResp { accepted: false };
    }
    PresenceReplicationResp { accepted: dht.store.put_presence_consent(&consent).unwrap_or(false) }
}

pub(crate) async fn handle_presence_state_rpc(
    dht: &Arc<Dht>, record: RelayPresenceState, authenticated_relay: NodeId, now_ms: u64,
) -> PresenceReplicationResp {
    if !record.verify(&authenticated_relay, now_ms)
        || !dht.store.has_presence_consent(&record.who.0, &record.recipient.0)
    {
        return PresenceReplicationResp { accepted: false };
    }
    let Ok(newest) = dht.store.put_presence_state(
        &record.recipient.0,
        &record.who.0,
        &record.state,
        record.version,
        record.observed_at_ms,
        record.lease.expires_at_ms,
    ) else {
        return PresenceReplicationResp { accepted: false };
    };
    if newest
        && let Some(conn) = dht
            .clients
            .as_ref()
            .and_then(|clients| clients.read().get(&record.recipient.0).cloned())
        && let Ok((mut tx, _)) = conn.open_bi().await
    {
        let _ = SRelayPacket::Presence(vec![PresenceP { who: record.who, state: record.state }])
            .send(&mut tx)
            .await;
        let _ = tx.finish();
    }
    PresenceReplicationResp { accepted: true }
}

pub(crate) async fn handle_presence_lease_rpc(
    dht: &Arc<Dht>, lease: PresenceLease, authenticated_relay: NodeId, now_ms: u64,
) -> PresenceReplicationResp {
    if lease.relay_id != authenticated_relay || !lease.verify(now_ms) {
        return PresenceReplicationResp { accepted: false };
    }
    PresenceReplicationResp { accepted: dht.store.put_presence_lease(&lease).unwrap_or(false) }
}

/// Lease-relay side of cross-relay live delivery. Never queues: failure tells
/// recipient homes to take their normal durable queue and push-wake path.
pub(crate) async fn handle_live_forward_rpc(
    dht: &Arc<Dht>, forward: LiveForward, authenticated_relay: NodeId, now_ms: u64,
) -> LiveForwardResp {
    if forward.dispatch.to.0 != forward.lease.user.0
        || forward.sender_relay_id != authenticated_relay
        || forward.lease.relay_id != dht.node_id
        || !forward.lease.verify(now_ms)
        || now_ms.abs_diff(forward.timestamp) > common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS
        || !verify_dispatch_user_sig(&forward.dispatch)
        || dht.presence_leases.as_ref().and_then(|leases| leases.read().get(&forward.lease.user.0).cloned())
            != Some(forward.lease.clone())
    {
        return LiveForwardResp { delivered: false };
    }
    let Some(sender_pubkey) = resolve_sender_pubkey(dht, &forward.sender_relay_id) else {
        return LiveForwardResp { delivered: false };
    };
    let msg = live_forward_signing_input(
        &forward.dispatch.id.0,
        &forward.lease,
        &forward.sender_relay_id,
        forward.timestamp,
    );
    if verify_ed25519(&sender_pubkey, &msg, &forward.sig.0).is_err() {
        return LiveForwardResp { delivered: false };
    }
    let conn = dht.clients.as_ref().and_then(|clients| clients.read().get(&forward.dispatch.to.0).cloned());
    let Some(conn) = conn else { return LiveForwardResp { delivered: false } };
    let delivery = dispatch_to_deliver(forward.dispatch);
    LiveForwardResp { delivered: try_deliver(&conn, &delivery).await.is_ok() }
}

/// Rejections are outcomes in the response, never a connection close, so the sender can attribute
/// failures to each home.
pub(crate) async fn handle_forward_rpc(dht: &Arc<Dht>, fwd: Forward, now_ms: u64) -> ForwardResp {
    let sender_pubkey = match resolve_sender_pubkey(dht, &fwd.sender_relay_id) {
        Some(pk) => pk,
        None => return ForwardResp { outcome: ForwardOutcome::BadSig },
    };

    if fwd.verify(&sender_pubkey, now_ms).is_err() {
        return ForwardResp { outcome: ForwardOutcome::BadSig };
    }

    let recipient_ipk: [u8; 32] = fwd.dispatch.to.0;
    if !super::routing::homes(dht, &NodeId::from_bytes(recipient_ipk)).1 {
        return ForwardResp { outcome: ForwardOutcome::NotOwner };
    }

    if !verify_dispatch_user_sig(&fwd.dispatch) {
        return ForwardResp { outcome: ForwardOutcome::BadSig };
    }

    // Live delivery: locally, or through the relay that holds the user's lease.
    let recipient_conn =
        dht.clients.as_ref().and_then(|map| map.read().get(&recipient_ipk).cloned());
    let live = async {
        if let Some(conn) = recipient_conn
            && try_deliver(&conn, &dispatch_to_deliver(fwd.dispatch.clone())).await.is_ok()
        {
            return true;
        }
        if is_primary_home(dht, &recipient_ipk)
            && let Some(lease) =
                dht.store.get_presence_lease(&recipient_ipk).filter(|lease| lease.verify(now_ms))
        {
            return forward_via_active_lease(dht, &fwd.dispatch, lease).await;
        }
        false
    };
    if timeout(Duration::from_millis(FORWARD_LIVE_DELIVERY_MS), live).await.unwrap_or(false) {
        return ForwardResp { outcome: ForwardOutcome::Delivered };
    }

    let outcome = super::store::enqueue_for_home(dht, &recipient_ipk, &fwd.dispatch, now_ms);
    if matches!(outcome, ForwardOutcome::Stored) {
        if dht.store.persist_barrier().wait().await.is_err() {
            return ForwardResp { outcome: ForwardOutcome::BadSig };
        }
        if fwd.dispatch.wake.wakes() {
            dht.trigger_wake(&recipient_ipk, fwd.dispatch.wake);
        }
    }
    ForwardResp { outcome }
}

/// Asks the lease relay to deliver to its local connection only. Any failure reads as an offline
/// recipient. Boxed: the dial spawns the peer serve loop, which reaches back here.
pub(crate) fn forward_via_active_lease<'a>(
    dht: &'a Arc<Dht>, dispatch: &'a DispatchP, lease: PresenceLease,
) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
    Box::pin(async move {
        let peer = dht.routing.read().get(&lease.relay_id).map(|entry| entry.descriptor());
        let Some(peer) = peer else { return false };
        let timestamp = now_ms();
        let sig = dht.signing_key.sign(&live_forward_signing_input(
            &dispatch.id.0, &lease, &dht.node_id, timestamp,
        ));
        let req = DhtRequest::LiveForward(LiveForward {
            dispatch: dispatch.clone(),
            lease,
            sender_relay_id: dht.node_id,
            timestamp,
            sig: sig.to_bytes().into(),
        });
        matches!(
            rpc(dht, &peer, &req, FORWARD_TIMEOUT_MS).await,
            Some(DhtResponse::LiveForward(LiveForwardResp { delivered: true }))
        )
    })
}

/// Exactly one home attempts live delivery. Other homes retain durable fallback.
fn is_primary_home(dht: &Dht, recipient: &[u8; 32]) -> bool {
    let nearest = dht.routing.read().closest(&NodeId::from_bytes(*recipient), 1);
    nearest.first().is_none_or(|peer| {
        xor32(dht.node_id.as_bytes(), recipient) < xor32(peer.id.as_bytes(), recipient)
    })
}

/// `peer_conns` covers a connected peer that did not fit in its routing bucket.
fn resolve_sender_pubkey(dht: &Dht, sender_relay_id: &NodeId) -> Option<[u8; 32]> {
    let known = |pk: &[u8; 32]| *pk != [0u8; 32];
    let routed = dht.routing.read().get(sender_relay_id).map(|entry| entry.pubkey).filter(known);
    routed.or_else(|| dht.peer_conns.read().get(sender_relay_id).map(|(_, pk)| *pk).filter(known))
}
