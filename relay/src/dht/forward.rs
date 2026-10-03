//! Sticky-home routing: the sender relay fans a dispatch out to the recipient's homes, the `K`
//! relays closest to the recipient's IPK.

use std::sync::Arc;

use common::crypto::verify_ed25519;
use common::proto::client_rel::ActivityP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::activity_sig_message;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::dht_p2p::ActivityForward;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::Forward;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::dht_p2p::ForwardResp;
use common::proto::dht_p2p::PresenceConsent;
use common::proto::dht_p2p::PresenceLease;
use common::proto::dht_p2p::RelayPresenceState;
use common::proto::dht_p2p::forward_signing_input;
use common::quic::id::NodeId;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use thiserror::Error;

use super::Dht;
use super::config::FORWARD_TIMEOUT_MS;
use super::config::write_quorum;
use super::home::forward_via_active_lease;
use super::home::handle_presence_lease_rpc;
use super::home::handle_presence_state_rpc;
use super::rpc::fan_out;

const ACTIVITY_MAX_SKEW_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HomeReply {
    pub node_id: NodeId,
    pub outcome: ForwardOutcome,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ForwardSummary {
    pub homes_tried:          Vec<NodeId>,
    pub delivered_at:         Vec<NodeId>,
    pub stored_at:            Vec<NodeId>,
    pub failed_at:            Vec<HomeReply>,
}

impl ForwardSummary {
    pub fn success_count(&self) -> usize {
        self.delivered_at.len() + self.stored_at.len()
    }

    pub fn any_delivered(&self) -> bool {
        !self.delivered_at.is_empty()
    }

    pub fn meets_k_min(&self) -> bool {
        self.success_count() >= write_quorum(self.homes_tried.len())
    }

    fn record(&mut self, node_id: NodeId, outcome: ForwardOutcome) {
        match outcome {
            ForwardOutcome::Delivered => self.delivered_at.push(node_id),
            ForwardOutcome::Stored => self.stored_at.push(node_id),
            other => self.failed_at.push(HomeReply { node_id, outcome: other }),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ForwardError {
    #[error("forward: insufficient replicas (wanted {wanted}, got {got})")]
    InsufficientReplicas { wanted: usize, got: usize, summary: Box<ForwardSummary> },
}

/// The caller has verified `dispatch.sig`. Homes check it again, since `Forward::verify` covers
/// only the relay's outer signature.
pub(crate) async fn forward_to_homes(
    dht: Arc<Dht>, dispatch: DispatchP, now_ms: u64,
) -> Result<ForwardSummary, ForwardError> {

    // A user's IPK is its DHT key as is, not hashed.
    let user_ipk_bytes: [u8; 32] = dispatch.to.0;
    let (descriptors, self_is_home) =
        super::routing::homes(&dht, &NodeId::from_bytes(user_ipk_bytes));

    let self_id = dht.node_id;
    let mut summary = ForwardSummary::default();
    let mut homes_tried: Vec<NodeId> = descriptors.iter().map(|peer| peer.id).collect();

    if self_is_home {
        homes_tried.push(self_id);
        let outcome = if let Some(lease) = dht
            .store
            .get_presence_lease(&user_ipk_bytes)
            .filter(|lease| lease.verify(now_ms))
            && forward_via_active_lease(&dht, &dispatch, lease).await
        {
            ForwardOutcome::Delivered
        } else {
            let stored = super::store::enqueue_for_home(&dht, &user_ipk_bytes, &dispatch, now_ms);
            if matches!(stored, ForwardOutcome::Stored)
                && dht.store.persist_barrier().wait().await.is_err()
            {
                ForwardOutcome::BadSig
            } else {
                stored
            }
        };
        // Only new content push-wakes; receipts/edits/etc. wait for drain.
        if matches!(outcome, ForwardOutcome::Stored) && dispatch.wake.wakes() {
            dht.trigger_wake(&user_ipk_bytes, dispatch.wake);
        }
        summary.record(self_id, outcome);
    }

    // One signature serves every home: the transcript does not name the home.
    let forward = DhtRequest::Forward(build_signed_forward(&dht, dispatch, now_ms));
    for (node_id, reply) in fan_out(&dht, &descriptors, &forward, FORWARD_TIMEOUT_MS).await {
        if let DhtResponse::Forward(ForwardResp { outcome }) = reply {
            summary.record(node_id, outcome);
        }
    }
    summary.homes_tried = homes_tried;

    let required = write_quorum(summary.homes_tried.len());
    if !summary.meets_k_min() {
        let got = summary.success_count();
        return Err(ForwardError::InsufficientReplicas {
            wanted: required,
            got,
            summary: Box::new(summary),
        });
    }

    Ok(summary)
}

/// Best effort, unlike message forwarding: an absent recipient misses the activity.
pub(crate) async fn forward_activity_to_homes(dht: Arc<Dht>, activity: ActivityP) {
    to_homes(&dht, activity.to.0, DhtRequest::ActivityForward(ActivityForward { activity })).await;
}

/// Replies are ignored: each caller is best effort.
async fn to_homes(dht: &Arc<Dht>, user: [u8; 32], req: DhtRequest) {
    let (homes, _) = super::routing::homes(dht, &NodeId::from_bytes(user));
    fan_out(dht, &homes, &req, FORWARD_TIMEOUT_MS).await;
}

pub(crate) async fn forward_presence_consent(dht: Arc<Dht>, consent: PresenceConsent) {
    to_homes(&dht, consent.recipient.0, DhtRequest::PresenceConsent(consent)).await;
}

/// The lease is user-signed, and homes also bind it to the relay that publishes it.
pub(crate) async fn forward_presence_lease(dht: Arc<Dht>, lease: PresenceLease) {
    let _ = handle_presence_lease_rpc(&dht, lease.clone(), dht.node_id, now_ms()).await;
    to_homes(&dht, lease.user.0, DhtRequest::PresenceLease(lease)).await;
}

pub(crate) async fn forward_presence_state(dht: Arc<Dht>, record: RelayPresenceState) {
    // Stored here too: this relay can be a recipient home, and state is one bounded row per pair.
    let _ = handle_presence_state_rpc(&dht, record.clone(), dht.node_id, now_ms()).await;
    to_homes(&dht, record.recipient.0, DhtRequest::PresenceState(record)).await;
}

fn build_signed_forward(dht: &Dht, dispatch: DispatchP, timestamp: u64) -> Forward {
    let sender_relay_id = dht.node_id;
    let msg = forward_signing_input(&dispatch.id.0, &sender_relay_id, timestamp);
    let sig = dht.signing_key.sign(&msg).to_bytes();
    Forward { dispatch, sender_relay_id, timestamp, sig: sig.into() }
}

/// Checked at ingress and again at each home: `Forward::verify` covers only the relay's signature.
pub(crate) fn verify_dispatch_user_sig(dispatch: &DispatchP) -> bool {
    let msg =
        dispatch_sig_message(&dispatch.to.0, &dispatch.from.0, &dispatch.id.0, &dispatch.payload);
    verify_ed25519(&dispatch.from.0, &msg, &dispatch.sig.0).is_ok()
}

pub(crate) fn activity_is_authentic(activity: &ActivityP, now_ms: u64) -> bool {
    if now_ms.abs_diff(activity.timestamp) > ACTIVITY_MAX_SKEW_MS {
        return false;
    }
    let msg = activity_sig_message(
        &activity.to,
        &activity.from,
        &activity.group_id,
        activity.activity,
        activity.timestamp,
    );
    verify_ed25519(&activity.from, &msg, &activity.sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quorum counts the homes selected before any RPC: a sole home is enough, and from two
    /// homes on, two must hold the dispatch. Rows: homes selected, delivered, stored, met.
    #[test]
    fn a_sole_home_is_a_quorum_and_otherwise_two_homes_must_hold_it() {
        let id = |n: u8| NodeId::from_bytes([n; 32]);
        for (selected, delivered, stored, met) in [
            (0, 0, 0, false),
            (1, 0, 1, true),
            (1, 0, 0, false),
            (2, 0, 1, false),
            (2, 1, 1, true),
            (2, 0, 2, true),
            (4, 1, 0, false),
            (4, 0, 2, true),
        ] {
            let mut summary = ForwardSummary {
                homes_tried: (0..selected).map(id).collect(),
                ..Default::default()
            };
            for n in 0..selected {
                let outcome = match n {
                    n if n < delivered => ForwardOutcome::Delivered,
                    n if n < delivered + stored => ForwardOutcome::Stored,
                    _ => ForwardOutcome::BadSig,
                };
                summary.record(id(n), outcome);
            }
            assert_eq!(
                summary.meets_k_min(),
                met,
                "{selected} homes, {delivered} delivered, {stored} stored"
            );
        }
    }
}
