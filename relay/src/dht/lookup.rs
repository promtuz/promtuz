//! Iterative `FindNode` walk, α requests per round. After its first reply, a round ends once
//! `LOOKUP_HEDGE_MS` passes without another.

use std::collections::HashSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FindNode;
use common::proto::dht_p2p::MAX_FIND_NODE_RESULTS;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::dht_hello_signing_input;
use common::proto::pack::Packer;
use common::quic::id::NodeId;
use common::quic::xor32;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use quinn::Connection;
use thiserror::Error;
use tokio::time::timeout;

use super::Dht;
use super::config::ALPHA;
use super::config::K;
use super::config::LOOKUP_HEDGE_MS;
use super::config::LOOKUP_MAX_HOPS;
use super::config::LOOKUP_RPC_TIMEOUT_MS;
use super::config::MAX_LOOKUP_CANDIDATES;
use super::routing::InsertOutcome;

#[derive(Debug, Error)]
pub enum LookupError {
    #[error("lookup: no candidates in routing table (bootstrap not done?)")]
    NoCandidates,

    #[error("lookup: timed out after {LOOKUP_RPC_TIMEOUT_MS}ms")]
    Timeout,

    #[error("lookup: exceeded {LOOKUP_MAX_HOPS} hops")]
    MaxHopsExceeded,
}

#[derive(Clone, Debug)]
struct Candidate {
    desc:     NodeDescriptor,
    distance: [u8; 32],
}

fn distance(target: &[u8; 32], peer: &NodeId) -> [u8; 32] {
    xor32(target, peer.as_bytes())
}

/// Peer-supplied addresses (`peer/N` dials, TURN targets) aim this relay's packets at a host of
/// the peer's choosing: special ranges are refused, loopback and private ones unless `allow_local`.
pub(crate) fn is_dialable_peer_addr(addr: &SocketAddr, allow_local: bool) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => {
            if v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_link_local()
                || v4.is_documentation()
            {
                return false;
            }
            allow_local || !(v4.is_loopback() || v4.is_private())
        },
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let head = v6.segments()[0];
            // fe80::/10 link-local.
            if head & 0xffc0 == 0xfe80 {
                return false;
            }
            // fc00::/7 unique-local is the v6 analogue of RFC1918.
            allow_local || !(v6.is_loopback() || head & 0xfe00 == 0xfc00)
        },
    }
}

/// A fresh dial pins the cert to `peer.id`, then sends our `DhtHello` without waiting for an ack:
/// a peer that rejects it closes the connection and the next RPC fails.
pub(crate) async fn connect_to_peer(
    dht: &Arc<Dht>, peer: &NodeDescriptor,
) -> anyhow::Result<Connection> {
    if let Some((conn, _pk)) = dht.peer_conns.read().get(&peer.id).cloned()
        && conn.close_reason().is_none() {
            return Ok(conn);
        }

    if !is_dialable_peer_addr(&peer.addr, dht.cfg.allow_local_peer_addrs) {
        return Err(anyhow::anyhow!(
            "refusing to dial {} at non-routable address {}",
            peer.id,
            peer.addr
        ));
    }

    let endpoint = match dht.endpoint.as_ref() {
        Some(ep) => ep.clone(),
        None => return Err(anyhow::anyhow!("DHT has no endpoint configured")),
    };
    let client_cfg = match dht.peer_client_cfg.as_ref() {
        Some(cfg) => cfg.clone(),
        None => return Err(anyhow::anyhow!("DHT has no peer_client_cfg configured")),
    };

    let sni = peer.id.to_string();
    let conn = endpoint
        .connect_with(client_cfg.as_ref().clone(), peer.addr, &sni)?
        .await?;

    let verified_pubkey = match crate::dht::tls_extract::extract_and_verify_pubkey(&conn, &peer.id) {
        Ok(pk) => pk,
        Err(e) => {
            common::warn!(
                "DHT connect_to_peer: post-handshake pubkey extraction failed for {}: {e}",
                peer.id
            );
            common::quic::CloseReason::DhtMalformedKey.close(&conn);
            return Err(anyhow::anyhow!(
                "post-handshake pubkey check failed for {}: {e}",
                peer.id
            ));
        }
    };

    if let Err(e) = send_dht_hello(dht, &conn).await {
        common::warn!(
            "DHT connect_to_peer: failed to send DhtHello to {}: {e}; closing",
            peer.id
        );
        common::quic::CloseReason::DhtMalformedKey.close(&conn);
        return Err(anyhow::anyhow!(
            "failed to send DhtHello to {}: {e}",
            peer.id
        ));
    }

    // A live connection cached by a concurrent dial wins over this one.
    {
        let mut conns = dht.peer_conns.write();
        if let Some((existing, _)) = conns.get(&peer.id).cloned()
            && existing.close_reason().is_none() {
                return Ok(existing);
            }
        conns.insert(peer.id, (conn.clone(), verified_pubkey));
    }
    let outcome = dht.routing.write().insert(NodeDescriptor {
        id: peer.id,
        addr: peer.addr,
        pubkey: verified_pubkey.into(),
    });
    probe_pending_ping(dht, outcome);

    // Serve the peer's RPCs on this connection too; a cached connection already has a serve loop.
    tokio::spawn(crate::dht::handler::serve_peer_streams(
        dht.clone(),
        conn.clone(),
        crate::dht::handler::AuthenticatedPeer::new(peer.id, verified_pubkey),
    ));

    Ok(conn)
}

async fn send_dht_hello(dht: &Arc<Dht>, conn: &Connection) -> anyhow::Result<()> {
    let node_id = dht.node_id;
    let pubkey: [u8; 32] = dht.signing_key.verifying_key().to_bytes();
    let timestamp = now_ms();
    let binding = common::quic::session_binding(conn, common::proto::dht_p2p::DHT_HELLO_EXPORTER_LABEL)?;
    let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp, &binding);
    let sig = dht.signing_key.sign(&msg).to_bytes();

    let hello = DhtHello {
        node_id,
        pubkey: Bytes(pubkey),
        timestamp,
        sig: Bytes(sig),
    };
    let bytes = hello.pack()?;

    let mut send = conn.open_uni().await?;
    send.write_all(&bytes).await?;
    send.finish()?;
    Ok(())
}

pub(crate) fn record_liveness(dht: &Dht, peer: &NodeId, alive: bool) {
    let mut routing = dht.routing.write();
    if alive {
        routing.ping_succeeded(peer);
    } else {
        routing.ping_failed(peer);
    }
}

/// `peer/N` has no PING, so a `FindNode` for the peer's own id is the liveness probe.
pub(crate) async fn probe_peer(dht: &Arc<Dht>, peer: &NodeDescriptor) -> bool {
    let req = DhtRequest::FindNode(FindNode {
        target:    (*peer.id.as_bytes()).into(),
        requester: dht.node_id,
    });
    let alive = matches!(
        super::rpc::rpc(dht, peer, &req, LOOKUP_RPC_TIMEOUT_MS).await,
        Some(DhtResponse::FindNode(_))
    );
    record_liveness(dht, &peer.id, alive);
    alive
}

/// A full bucket hands back its LRU entry, which frees a slot only once probed dead. Detached
/// because every insert site is latency-sensitive.
pub(crate) fn probe_pending_ping(dht: &Arc<Dht>, outcome: InsertOutcome) {
    let InsertOutcome::PendingPing(lru) = outcome else {
        return;
    };
    let dht = dht.clone();
    tokio::spawn(async move {
        probe_peer(&dht, &lru).await;
    });
}

pub(crate) async fn lookup_node(
    dht: Arc<Dht>, target: NodeId,
) -> Result<Vec<NodeDescriptor>, LookupError> {

    let target_bytes = *target.as_bytes();

    let initial: Vec<NodeDescriptor> = {
        let routing = dht.routing.read();
        routing.find_closest(&target, K * 2)
    };

    if initial.is_empty() {
        return Err(LookupError::NoCandidates);
    }

    let mut candidates: Vec<Candidate> = initial
        .into_iter()
        .map(|desc| {
            let distance = distance(&target_bytes, &desc.id);
            Candidate { desc, distance }
        })
        .collect();
    candidates.sort_by_key(|a| a.distance);

    let mut queried: HashSet<NodeId> = HashSet::new();
    queried.insert(dht.node_id); // never query self
    let mut closest_so_far: Vec<Candidate> = Vec::with_capacity(K);

    let deadline = Instant::now() + Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS);
    let mut hops: u32 = 0;

    let res = run_iterative_loop(
        &dht,
        &target_bytes,
        &mut candidates,
        &mut queried,
        &mut closest_so_far,
        &mut hops,
        deadline,
    )
    .await;

    res.map(|_| closest_so_far.into_iter().take(K).map(|c| c.desc).collect())
}

#[allow(clippy::too_many_arguments)]
async fn run_iterative_loop(
    dht: &Arc<Dht>, target: &[u8; 32], candidates: &mut Vec<Candidate>,
    queried: &mut HashSet<NodeId>, closest_so_far: &mut Vec<Candidate>, hops: &mut u32,
    deadline: Instant,
) -> Result<(), LookupError> {
    use tokio::task::JoinSet;

    loop {
        if Instant::now() >= deadline {
            return Err(LookupError::Timeout);
        }
        if *hops >= LOOKUP_MAX_HOPS {
            return Err(LookupError::MaxHopsExceeded);
        }

        let mut batch: Vec<NodeDescriptor> = Vec::with_capacity(ALPHA);
        for c in candidates.iter() {
            if !queried.contains(&c.desc.id) {
                batch.push(c.desc.clone());
                if batch.len() >= ALPHA {
                    break;
                }
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut set: JoinSet<RpcResult> = JoinSet::new();
        for desc in batch.iter() {
            queried.insert(desc.id);
            let dht_ref = dht.clone();
            let desc_clone = desc.clone();
            let target_arr = *target;
            set.spawn(async move {
                send_one_hop(&dht_ref, desc_clone, target_arr).await
            });
        }

        let hedge_window = Duration::from_millis(LOOKUP_HEDGE_MS);
        let mut got_one = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                set.abort_all();
                return Err(LookupError::Timeout);
            }
            let wait = if got_one { hedge_window.min(remaining) } else { remaining };

            let joined = match timeout(wait, set.join_next()).await {
                Ok(j) => j,
                Err(_) => {
                    if got_one {
                        break;
                    } else {
                        continue;
                    }
                }
            };
            let Some(task_result) = joined else {
                break;
            };
            let result = match task_result {
                Ok(r) => r,
                Err(_join) => continue,
            };
            got_one = true;
            match result {
                RpcResult::FindNodeReply(closer) => {
                    integrate_descriptors(
                        target,
                        candidates,
                        &closer,
                        dht.cfg.allow_local_peer_addrs,
                    );
                }
                RpcResult::Failed => {}
            }
        }

        *hops += 1;
        candidates.sort_by_key(|a| a.distance);
        candidates.truncate(MAX_LOOKUP_CANDIDATES);
        closest_so_far.clear();
        for c in candidates.iter().take(K) {
            closest_so_far.push(c.clone());
        }
        let farthest = closest_so_far.last().map(|c| c.distance);
        let any_closer_unqueried = candidates.iter().any(|c| {
            !queried.contains(&c.desc.id)
                && farthest.map(|f| c.distance < f).unwrap_or(true)
        });
        if !any_closer_unqueried {
            return Ok(());
        }
    }
}
enum RpcResult {
    FindNodeReply(Vec<NodeDescriptor>),
    Failed,
}

async fn send_one_hop(
    dht: &Arc<Dht>, peer: NodeDescriptor, target: [u8; 32],
) -> RpcResult {
    let req = DhtRequest::FindNode(FindNode {
        target:    target.into(),
        requester: dht.node_id,
    });
    let resp = super::rpc::rpc(dht, &peer, &req, LOOKUP_RPC_TIMEOUT_MS).await;
    record_liveness(dht, &peer.id, resp.is_some());
    match resp {
        Some(DhtResponse::FindNode(r)) => RpcResult::FindNodeReply(r.closer),
        _ => RpcResult::Failed,
    }
}

/// Caps `new` itself, so a hop's growth stays bounded however the reply was decoded.
fn integrate_descriptors(
    target: &[u8; 32], candidates: &mut Vec<Candidate>, new: &[NodeDescriptor],
    allow_local: bool,
) {
    let known: HashSet<NodeId> = candidates.iter().map(|c| c.desc.id).collect();
    for desc in new.iter().take(MAX_FIND_NODE_RESULTS) {
        if known.contains(&desc.id) || !is_dialable_peer_addr(&desc.addr, allow_local) {
            continue;
        }
        let dist = distance(target, &desc.id);
        candidates.push(Candidate { desc: desc.clone(), distance: dist });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SSRF: a peer-advertised address is never a special range, and is loopback or private
    /// only on a single-host test cluster. Rows: addresses, then dialable (by default, locally).
    #[test]
    fn peer_addresses_are_dialed_only_when_routable() {
        let rows: [(&[&str], (bool, bool)); 3] = [
            (
                &[
                    "0.0.0.0:4433",
                    "224.0.0.1:4433",
                    "255.255.255.255:4433",
                    "169.254.1.1:4433",
                    "192.0.2.1:4433",
                    "93.184.216.34:0",
                    "[::]:4433",
                    "[ff02::1]:4433",
                    "[fe80::1]:4433",
                ],
                (false, false),
            ),
            (
                &[
                    "127.0.0.1:4433",
                    "10.1.2.3:4433",
                    "192.168.0.5:4433",
                    "[::1]:4433",
                    "[fd00::1]:4433",
                ],
                (false, true),
            ),
            (&["93.184.216.34:4433", "[2606:4700::1111]:4433"], (true, true)),
        ];
        for (addrs, expected) in rows {
            for addr in addrs {
                let addr: SocketAddr = addr.parse().unwrap();
                let dialable =
                    (is_dialable_peer_addr(&addr, false), is_dialable_peer_addr(&addr, true));
                assert_eq!(dialable, expected, "{addr}");
            }
        }
    }

    #[test]
    fn a_lookup_reply_adds_only_dialable_peers_up_to_the_wire_bound() {
        let reply: Vec<NodeDescriptor> = (0..MAX_FIND_NODE_RESULTS as u8 + 3)
            .map(|n| NodeDescriptor {
                id:     NodeId::new([n; 32]),
                addr:   if n == 0 { "127.0.0.1:4433" } else { "93.184.216.34:4433" }
                    .parse()
                    .unwrap(),
                pubkey: [n; 32].into(),
            })
            .collect();
        let mut candidates = Vec::new();
        integrate_descriptors(&[0; 32], &mut candidates, &reply, false);
        let added: Vec<NodeId> = candidates.iter().map(|c| c.desc.id).collect();
        let expected: Vec<NodeId> = reply[1..MAX_FIND_NODE_RESULTS].iter().map(|d| d.id).collect();
        assert_eq!(added, expected);
    }
}
