//! Inbound `peer/5` connections. A signed `DhtHello` on the first uni-stream authenticates the
//! dialer, then each bi-stream carries one RPC.

use std::sync::Arc;
use std::time::Duration;

use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtHelloVerifyError;
use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FindNodeResp;
use common::proto::dht_p2p::MAX_FIND_NODE_RESULTS;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::mls_wire::KeyPackageFetchResp;
use common::proto::mls_wire::KeyPackagePublishResp;
use common::proto::mls_wire::KeyPackageRefillResp;
use common::proto::mls_wire::KpPublishMode;
use common::proto::mls_wire::WelcomeFetchResp;
use common::proto::mls_wire::WelcomePublishResp;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use common::quic::id::NodeId;
use common::utils::now_ms;
use quinn::Connection;
use quinn::SendStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::Dht;
use super::mls::kp;
use super::mls::welcome;
use super::rate_limit::RpcClass;
use super::routing::RoutingTable;

const MAX_CONCURRENT_STREAMS_PER_PEER: usize = 16;

const HELLO_RECV_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn handle_peer_connection(dht: Arc<Dht>, conn: Connection) {
    // A failed hello has already closed the connection with the matching reason.
    let Ok(auth) = recv_and_verify_hello(&conn).await else { return };

    // Routable from now on, before the peer sends any RPC.
    {
        let desc = NodeDescriptor {
            id: auth.node_id,
            addr: conn.remote_address(),
            pubkey: auth.pubkey.into(),
        };
        let outcome = dht.routing.write().insert(desc);
        super::lookup::probe_pending_ping(&dht, outcome);
    }
    {
        let mut map = dht.peer_conns.write();
        // The first cached connection wins, so reconnect storms do not churn the cache.
        map.entry(auth.node_id).or_insert_with(|| (conn.clone(), auth.pubkey));
    }

    serve_peer_streams(dht, conn, auth).await;
}

/// Serves accepted and dialed connections alike, so the one cached connection per peer works in
/// both directions.
pub(crate) async fn serve_peer_streams(dht: Arc<Dht>, conn: Connection, auth: AuthenticatedPeer) {
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS_PER_PEER));
    let conn_id = conn.stable_id();

    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let (send, recv) = stream;

        let permit = match limiter.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let dht_clone = dht.clone();
        let conn_for_task = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut recv = recv;
            handle_one_stream(dht_clone, conn_for_task, send, &mut recv, auth).await;
        });
    }

    // Evict only if the cache still holds this connection; a reconnect may have replaced it.
    let peer_id_to_remove: Option<NodeId> = {
        let map = dht.peer_conns.read();
        map.iter().find_map(
            |(id, (c, _pk))| {
                if c.stable_id() == conn_id { Some(*id) } else { None }
            },
        )
    };
    if let Some(id) = peer_id_to_remove {
        let mut map = dht.peer_conns.write();
        if let Some((c, _pk)) = map.get(&id)
            && c.stable_id() == conn_id
        {
            map.remove(&id);
        }
    }
}

/// The connection's peer identity, from the `DhtHello` or the dial's pin. Fixed for the
/// connection's lifetime.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthenticatedPeer {
    node_id: NodeId,
    pubkey: [u8; 32],
}

impl AuthenticatedPeer {
    /// For a dialed connection, already authenticated by the cert pin; no `DhtHello` comes back.
    pub(crate) fn new(node_id: NodeId, pubkey: [u8; 32]) -> Self {
        Self { node_id, pubkey }
    }
}

async fn recv_and_verify_hello(conn: &Connection) -> Result<AuthenticatedPeer, ()> {
    let mut recv = match timeout(HELLO_RECV_TIMEOUT, conn.accept_uni()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // Connection died before any frame arrived; nothing is left to close.
            common::debug!(
                "DHT inbound from {}: connection ended before DhtHello: {e}",
                conn.remote_address()
            );
            return Err(());
        },
        Err(_) => {
            // No hello in time. `DhtClockSkew` is reused: the peer missed its window.
            common::warn!(
                "DHT inbound from {}: no DhtHello within {:?}; closing",
                conn.remote_address(),
                HELLO_RECV_TIMEOUT
            );
            CloseReason::DhtClockSkew.close(conn);
            return Err(());
        },
    };

    let hello: DhtHello = match DhtHello::unpack(&mut recv).await {
        Ok(h) => h,
        Err(e) => {
            common::warn!(
                "DHT inbound from {}: malformed DhtHello frame: {e}",
                conn.remote_address()
            );
            CloseReason::DhtMalformedKey.close(conn);
            return Err(());
        },
    };

    // Verify (id-binding, pubkey shape, signature over this session, timestamp window).
    let now = now_ms();
    let Ok(binding) =
        common::quic::session_binding(conn, common::proto::dht_p2p::DHT_HELLO_EXPORTER_LABEL)
    else {
        CloseReason::DhtBadSignature.close(conn);
        return Err(());
    };
    match verify_hello_with_close_reason(&hello, now, &binding) {
        Ok(()) => Ok(AuthenticatedPeer { node_id: hello.node_id, pubkey: hello.pubkey.0 }),
        Err(reason) => {
            common::warn!(
                "DHT inbound from {} failed DhtHello verification; closing with {:?}",
                conn.remote_address(),
                reason
            );
            reason.close(conn);
            Err(())
        },
    }
}

fn verify_hello_with_close_reason(
    hello: &DhtHello, now_ms: u64, binding: &[u8; 32],
) -> Result<(), CloseReason> {
    hello.verify(now_ms, binding).map_err(|e| match e {
        DhtHelloVerifyError::IdMismatch | DhtHelloVerifyError::MalformedPubkey => {
            CloseReason::DhtMalformedKey
        },
        DhtHelloVerifyError::BadSignature => CloseReason::DhtBadSignature,
        DhtHelloVerifyError::ClockSkew => CloseReason::DhtClockSkew,
    })
}

async fn handle_one_stream(
    dht: Arc<Dht>, conn: Connection, mut send: SendStream, recv: &mut quinn::RecvStream,
    auth: AuthenticatedPeer,
) {
    let pkt = match DhtPacket::unpack(recv).await {
        Ok(p) => p,
        Err(_) => {
            CloseReason::DhtMalformedKey.close(&conn);
            return;
        },
    };
    let req = match pkt {
        DhtPacket::Request(r) => r,
        DhtPacket::Response(_) => {
            CloseReason::PacketMismatch.close(&conn);
            return;
        },
    };

    // Keyed on the authenticated NodeId, so reconnecting does not reset a peer's quota.
    let class = RpcClass::for_request(&req);
    if dht.rate_limiters.check(&auth.node_id, class).is_err() {
        common::warn!(
            "DHT inbound rate limit tripped (peer={}, class={class:?}); closing connection",
            auth.node_id
        );
        CloseReason::DhtFlood.close(&conn);
        return;
    }

    let resp = handle_dht_request(&dht, req, auth.node_id).await;

    // Usually a refresh: setting up the connection already inserted the peer.
    {
        let desc = NodeDescriptor {
            id: auth.node_id,
            addr: conn.remote_address(),
            pubkey: auth.pubkey.into(),
        };
        let outcome = dht.routing.write().insert(desc);
        super::lookup::probe_pending_ping(&dht, outcome);
    }

    let bytes = match DhtPacket::Response(resp).pack() {
        Ok(b) => b,
        Err(_) => {
            CloseReason::DhtMalformedKey.close(&conn);
            return;
        },
    };
    if send.write_all(&bytes).await.is_err() {
        return;
    }
    let _ = send.finish();
}

pub(crate) async fn handle_dht_request(
    dht: &Arc<Dht>, req: DhtRequest, authenticated_peer_id: NodeId,
) -> DhtResponse {
    match req {
        DhtRequest::ServiceCapabilities => DhtResponse::ServiceCapabilities { supported: super::mls::service_support().encode().into() },
        DhtRequest::KeyPackageInventory { request } => DhtResponse::KeyPackageInventory {
            inventory: super::mls::inventory::handle(dht, &request.0, authenticated_peer_id, now_ms()).await.into(),
        },
        DhtRequest::FindNode(f) => {
            let target_id = NodeId::from_bytes(f.target.0);
            let closer = closest_excluding(&dht.routing.read(), &target_id, &f.requester);
            DhtResponse::FindNode(FindNodeResp { closer })
        },
        DhtRequest::Forward(fwd) => {
            DhtResponse::Forward(super::home::handle_forward_rpc(dht, fwd, now_ms()).await)
        },
        DhtRequest::ActivityForward(activity) => DhtResponse::ActivityForward(
            super::home::handle_activity_forward_rpc(dht, activity, now_ms()).await,
        ),
        DhtRequest::PresenceConsent(consent) => DhtResponse::PresenceConsent(
            super::home::handle_presence_consent_rpc(dht, consent, now_ms()).await,
        ),
        DhtRequest::PresenceState(state) => DhtResponse::PresenceState(
            super::home::handle_presence_state_rpc(dht, state, authenticated_peer_id, now_ms())
                .await,
        ),
        DhtRequest::PresenceLease(lease) => DhtResponse::PresenceLease(
            super::home::handle_presence_lease_rpc(dht, lease, authenticated_peer_id, now_ms())
                .await,
        ),
        DhtRequest::LiveForward(forward) => DhtResponse::LiveForward(
            super::home::handle_live_forward_rpc(dht, forward, authenticated_peer_id, now_ms()).await,
        ),
        DhtRequest::PushPseudonymPublish(publish) => DhtResponse::PushPseudonymPublish(
            super::push_replication::handle_publish(dht, publish, now_ms()),
        ),
        DhtRequest::QueueFetch(req) => DhtResponse::QueueFetch(
            super::queue_drain::handle_queue_fetch_rpc(dht, req, authenticated_peer_id, now_ms())
                .await,
        ),
        DhtRequest::QueueFetchAck(req) => DhtResponse::QueueFetchAck(
            super::queue_drain::handle_queue_fetch_ack_rpc(
                dht,
                req,
                authenticated_peer_id,
                now_ms(),
            )
            .await,
        ),
        DhtRequest::KeyPackagePublish(r) => DhtResponse::KeyPackagePublish(KeyPackagePublishResp {
            outcome: kp::handle_keypackage_publish(
                dht, &r.ipk.0, &r.records, r.timestamp, &r.sig.0, KpPublishMode::Publish, now_ms(),
            )
            .await,
        }),
        DhtRequest::KeyPackageFetch(req) => DhtResponse::KeyPackageFetch(KeyPackageFetchResp {
            outcome: kp::handle_keypackage_fetch(dht, req, authenticated_peer_id, now_ms()).await,
        }),
        DhtRequest::KeyPackageRefill(r) => DhtResponse::KeyPackageRefill(KeyPackageRefillResp {
            outcome: kp::refill_outcome(
                kp::handle_keypackage_publish(
                    dht, &r.ipk.0, &r.records, r.timestamp, &r.sig.0, KpPublishMode::Refill, now_ms(),
                )
                .await,
            ),
        }),
        DhtRequest::WelcomePublish(req) => DhtResponse::WelcomePublish(WelcomePublishResp {
            outcome: welcome::handle_welcome_publish(dht, req, authenticated_peer_id, now_ms()).await,
        }),
        DhtRequest::WelcomeFetch(req) => DhtResponse::WelcomeFetch(WelcomeFetchResp {
            outcome: welcome::handle_welcome_fetch(dht, req, authenticated_peer_id, now_ms()),
        }),
        DhtRequest::WelcomeAck(req) => DhtResponse::WelcomeAck(welcome::handle_welcome_ack(
            dht,
            req,
            authenticated_peer_id,
            now_ms(),
        )),
    }
}

fn closest_excluding(
    routing: &RoutingTable, target: &NodeId, exclude: &NodeId,
) -> Vec<NodeDescriptor> {
    routing
        .find_closest(target, MAX_FIND_NODE_RESULTS + 1)
        .into_iter()
        .filter(|d| &d.id != exclude)
        .take(MAX_FIND_NODE_RESULTS)
        .collect()
}

#[cfg(test)]
mod tests {
    use common::proto::client_rel::ActivityP;
    use common::proto::client_rel::DispatchP;
    use common::proto::client_rel::activity_sig_message;
    use common::proto::dht_p2p::Forward;
    use common::proto::dht_p2p::ForwardOutcome;
    use common::proto::dht_p2p::ForwardResp;
    use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
    use common::proto::dht_p2p::MAX_FETCH_QUEUE_BATCH;
    use common::proto::dht_p2p::QueueFetch;
    use common::proto::dht_p2p::QueueFetchAck;
    use common::proto::dht_p2p::QueueFetchAckResp;
    use common::proto::dht_p2p::QueueFetchResp;
    use common::proto::dht_p2p::dht_hello_signing_input;
    use common::proto::dht_p2p::forward_signing_input;
    use common::proto::dht_p2p::queue_fetch_ack_signing_input;
    use common::proto::dht_p2p::queue_fetch_signing_input;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::forward::activity_is_authentic;
    use crate::quic::handler::client::events::drain_auth::DrainAuthError;
    use crate::quic::handler::client::events::drain_auth::verify_drain_auth;
    use crate::test_support::dht;
    use crate::test_support::dispatch;
    use crate::test_support::key;
    use crate::test_support::put_queued;
    use crate::test_support::queued;

    fn forward(relay: &SigningKey, sent: &DispatchP, now: u64) -> DhtRequest {
        let sender_relay_id = NodeId::new(relay.verifying_key().to_bytes());
        let sig = relay.sign(&forward_signing_input(&sent.id.0, &sender_relay_id, now)).to_bytes();
        DhtRequest::Forward(Forward {
            dispatch: sent.clone(),
            sender_relay_id,
            timestamp: now,
            sig: sig.into(),
        })
    }

    fn fetch(user: &SigningKey, requester: NodeId, now: u64) -> DhtRequest {
        let user_ipk = user.verifying_key().to_bytes();
        let sig = user.sign(&queue_fetch_signing_input(&user_ipk, &requester, now)).to_bytes();
        DhtRequest::QueueFetch(QueueFetch {
            user_ipk:           user_ipk.into(),
            requester_relay_id: requester,
            timestamp:          now,
            user_sig:           sig.into(),
        })
    }

    fn ack(user: &SigningKey, requester: NodeId, ids: Vec<[u8; 16]>, now: u64) -> DhtRequest {
        let user_ipk = user.verifying_key().to_bytes();
        let sig =
            user.sign(&queue_fetch_ack_signing_input(&user_ipk, &requester, &ids, now)).to_bytes();
        DhtRequest::QueueFetchAck(QueueFetchAck {
            user_ipk:           user_ipk.into(),
            requester_relay_id: requester,
            delivered_ids:      ids,
            timestamp:          now,
            user_sig:           sig.into(),
        })
    }

    fn stored(outcome: ForwardOutcome) -> DhtResponse {
        DhtResponse::Forward(ForwardResp { outcome })
    }

    fn fetched(messages: Vec<DispatchP>, exhausted: bool) -> DhtResponse {
        DhtResponse::QueueFetch(QueueFetchResp { messages, exhausted })
    }

    fn acked(ok: bool) -> DhtResponse {
        DhtResponse::QueueFetchAck(QueueFetchAckResp { ok })
    }

    /// Home custody at the RPC boundary: both signature layers and ownership guard a store, the
    /// batch cap reports what is left, and a fetch or ack replayed by another relay leaks and
    /// deletes nothing. Rows: request, authenticated peer, response, then the queue after it.
    #[tokio::test]
    async fn a_home_stores_serves_and_deletes_only_for_signed_owned_requests() {
        let (_dir, home) = dht(NodeId::from_bytes([0xFF; 32]));
        let relay = key(1);
        let relay_id = NodeId::new(relay.verifying_key().to_bytes());
        home.routing.write().insert(NodeDescriptor {
            id:     relay_id,
            addr:   "127.0.0.1:1".parse().unwrap(),
            pubkey: relay.verifying_key().to_bytes().into(),
        });
        let (drainer, replayer) = (NodeId::from_bytes([0x77; 32]), NodeId::from_bytes([0x88; 32]));
        let (bob, now) = (key(2), now_ms());
        let bob_ipk = bob.verifying_key().to_bytes();
        let sent = dispatch(&key(3), bob_ipk, [1; 16], b"offline");
        let mut forged_relay = forward(&relay, &sent, now);
        if let DhtRequest::Forward(f) = &mut forged_relay {
            f.sig.0[0] ^= 1;
        }
        let mut forged_user = sent.clone();
        forged_user.sig.0[0] ^= 1;
        let mut bad_ack = ack(&bob, drainer, vec![[1; 16]], now);
        if let DhtRequest::QueueFetchAck(a) = &mut bad_ack {
            a.user_sig.0[0] ^= 1;
        }

        let rows = [
            (forged_relay, relay_id, stored(ForwardOutcome::BadSig), vec![]),
            (forward(&relay, &forged_user, now), relay_id, stored(ForwardOutcome::BadSig), vec![]),
            (forward(&key(4), &sent, now), relay_id, stored(ForwardOutcome::BadSig), vec![]),
            (forward(&relay, &sent, now), relay_id, stored(ForwardOutcome::Stored), vec![[1; 16]]),
            (fetch(&bob, drainer, now), replayer, fetched(vec![], true), vec![[1; 16]]),
            (fetch(&bob, drainer, now), drainer, fetched(vec![sent.clone()], true), vec![[1; 16]]),
            (ack(&bob, drainer, vec![[1; 16]], now), replayer, acked(false), vec![[1; 16]]),
            (bad_ack, drainer, acked(false), vec![[1; 16]]),
            (ack(&bob, drainer, vec![[1; 16]], now), drainer, acked(true), vec![]),
        ];
        for (n, (request, peer, response, after)) in rows.into_iter().enumerate() {
            assert_eq!(handle_dht_request(&home, request, peer).await, response, "row {n}");
            assert_eq!(queued(&home, &bob_ipk), after, "row {n}");
        }

        // `exhausted` is what tells the drainer to fetch again for the rest.
        for (seed, rows, exhausted) in
            [(5, MAX_FETCH_QUEUE_BATCH + 5, false), (6, MAX_FETCH_QUEUE_BATCH, true)]
        {
            let (user, user_ipk) = (key(seed), key(seed).verifying_key().to_bytes());
            for n in 0..rows {
                let mut id = [0; 16];
                id[..8].copy_from_slice(&(n as u64).to_be_bytes());
                put_queued(
                    &home,
                    &user_ipk,
                    now + n as u64,
                    &dispatch(&key(7), user_ipk, id, b"x"),
                );
            }
            let DhtResponse::QueueFetch(batch) =
                handle_dht_request(&home, fetch(&user, drainer, now), drainer).await
            else {
                panic!("not a fetch reply")
            };
            assert_eq!((batch.messages.len(), batch.exhausted), (MAX_FETCH_QUEUE_BATCH, exhausted));
        }

        // Three relays closer to this recipient make this one no longer its home.
        for n in 1..=3 {
            let mut id = [0; 32];
            id[31] = n;
            home.routing.write().insert(NodeDescriptor {
                id:     NodeId::from_bytes(id),
                addr:   "127.0.0.1:1".parse().unwrap(),
                pubkey: [0; 32].into(),
            });
        }
        let elsewhere = dispatch(&key(3), [0; 32], [2; 16], b"not mine");
        let response = handle_dht_request(&home, forward(&relay, &elsewhere, now), relay_id).await;
        assert_eq!(response, stored(ForwardOutcome::NotOwner));
        assert!(queued(&home, &[0; 32]).is_empty());
    }

    /// The relay's own signature checks refuse what is stale, forged, or bound to another
    /// session, relay, recipient or group.
    #[test]
    fn relay_verifiers_refuse_stale_forged_and_rebound_requests() {
        let now = 1_700_000_000_000;
        let session = [0x5B; 32];
        let peer = key(1);
        let hello = |timestamp: u64, binding: [u8; 32]| {
            let pubkey = peer.verifying_key().to_bytes();
            let node_id = NodeId::new(pubkey);
            let sig = peer.sign(&dht_hello_signing_input(&node_id, &pubkey, timestamp, &binding));
            DhtHello { node_id, pubkey: pubkey.into(), timestamp, sig: sig.to_bytes().into() }
        };
        let mut forged = hello(now, session);
        forged.sig.0[0] ^= 1;
        let mut claimed = hello(now, session);
        claimed.node_id = NodeId::new(key(2).verifying_key().to_bytes());
        for (label, sent, refusal) in [
            ("fresh", hello(now, session), None),
            ("two minutes old", hello(now - 120_000, session), Some("DhtClockSkew")),
            ("forged", forged, Some("DhtBadSignature")),
            ("signed on another session", hello(now, [0x5C; 32]), Some("DhtBadSignature")),
            ("someone else's id", claimed, Some("DhtMalformedKey")),
        ] {
            let result = verify_hello_with_close_reason(&sent, now, &session)
                .err()
                .map(|r| format!("{r:?}"));
            assert_eq!(result.as_deref(), refusal, "hello: {label}");
        }

        let (user, relay) = (key(3), NodeId::from_bytes([4; 32]));
        let drain = |signed_for: &NodeId, timestamp: u64| {
            let ipk = user.verifying_key();
            let sig = user.sign(&queue_fetch_signing_input(ipk.as_bytes(), signed_for, timestamp));
            verify_drain_auth(&ipk, &relay, now, timestamp, sig.to_bytes()).map(|_| ())
        };
        for (label, result, expected) in [
            ("fresh", drain(&relay, now), Ok(())),
            ("at the skew limit", drain(&relay, now - MAX_DHT_HELLO_SKEW_MS), Ok(())),
            (
                "past the skew limit",
                drain(&relay, now - MAX_DHT_HELLO_SKEW_MS - 1),
                Err(DrainAuthError::StaleTimestamp),
            ),
            (
                "from the future",
                drain(&relay, now + MAX_DHT_HELLO_SKEW_MS + 1),
                Err(DrainAuthError::FutureTimestamp),
            ),
            (
                "for another relay",
                drain(&NodeId::from_bytes([5; 32]), now),
                Err(DrainAuthError::BadSig),
            ),
        ] {
            assert_eq!(result, expected, "drain auth: {label}");
        }

        let sender = key(6);
        let activity = |to: [u8; 32], group: [u8; 32], timestamp: u64| {
            let from = sender.verifying_key().to_bytes();
            let sig = sender.sign(&activity_sig_message(&[7; 32], &from, &[8; 32], 1, timestamp));
            ActivityP {
                to: to.into(),
                from: from.into(),
                group_id: group.into(),
                activity: 1,
                timestamp,
                sig: sig.to_bytes().into(),
            }
        };
        for (label, sent, accepted) in [
            ("fresh", activity([7; 32], [8; 32], now), true),
            ("stale", activity([7; 32], [8; 32], now - 30_001), false),
            ("moved to another recipient", activity([9; 32], [8; 32], now), false),
            ("moved to another group", activity([7; 32], [9; 32], now), false),
        ] {
            assert_eq!(activity_is_authentic(&sent, now), accepted, "activity: {label}");
        }
    }
}
