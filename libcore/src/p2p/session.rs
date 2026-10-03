//! One connection attempt to a peer, from the offer exchange to a verified, published link.

use std::collections::HashSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::p2p_relay::RelayMsg;
use common::utils::now_ms;
use quinn::Connection;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::ACCEPT_TIMEOUT;
use super::AbortGuard;
use super::DIAL_TIMEOUT;
use super::Disclosure;
use super::InboundGuard;
use super::OFFER_TTL_MS;
use super::Offer;
use super::OfferGuard;
use super::P2pEndpoint;
use super::PeerLink;
use super::PokeGuard;
use super::RouteGuards;
use super::SIGNAL_TIMEOUT;
use super::TurnGuard;
use super::candidate;
use super::diagnostics;
use super::diagnostics::Event;
use super::disco::DiscoKey;
use super::punch;
use super::signal;
use super::socket::Poke;
use super::tcp;
use crate::data::identity::Identity;
use crate::state::core;
use crate::utils::addr_short;
use crate::utils::addrs_short;

/// Unchecked: identity comes from [`crate::transfer::auth`], which binds the IPK to the TLS key.
const PEER_SNI: &str = "peer";
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);
/// How long an offer waits for the reflexive probe, so it can carry that candidate.
const REFLEXIVE_WAIT: Duration = Duration::from_millis(600);
const REFLEXIVE_MAX_AGE: Duration = Duration::from_secs(60);

/// A public tag derived from the sorted IPKs, so both ends agree on it before any offer.
fn channel_for(a: &[u8; 32], b: &[u8; 32]) -> [u8; 8] {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"promtuz/p2p/chan");
    hasher.update(lo);
    hasher.update(hi);
    let mut chan = [0u8; 8];
    chan.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    chan
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// Falls back to the connected relay even without UDP assist: its authenticated tunnel may still
/// grant TCP assist. The `assist` flag means UDP assist only.
fn assist_relay() -> Option<SocketAddr> {
    let live = core()
        .session()
        .filter(|s| s.conn.close_reason().is_none())
        .map(|s| (s.relay.host.to_string(), s.relay.port, s.relay.assist));
    let (host, port) = match live.as_ref().filter(|(_, _, assist)| *assist) {
        Some((host, port, _)) => (host.clone(), *port),
        None => match crate::data::relay::Relay::fetch_assist_capable() {
            Some(r) => (r.host.to_string(), r.port),
            None => {
                let (host, port, _) = live?;
                (host, port)
            },
        },
    };
    let ip: IpAddr = host.parse().ok()?;
    Some(SocketAddr::new(ip, port))
}

async fn refresh_reflexive(ep: Arc<P2pEndpoint>) -> Option<SocketAddr> {
    let relay = assist_relay()?;
    if let Some(addr) = ep.reflexive.lock().cached(relay, REFLEXIVE_MAX_AGE) {
        return Some(addr);
    }
    let tx = rand_bytes::<8>();
    let reply = ep.reflexive.lock().begin(tx, relay, REFLEXIVE_WAIT)?;
    struct ProbeGuard(Arc<P2pEndpoint>, [u8; 8]);
    impl Drop for ProbeGuard {
        fn drop(&mut self) { self.0.reflexive.lock().cancel(&self.1); }
    }
    let _guard = ProbeGuard(ep.clone(), tx);
    timeout(REFLEXIVE_WAIT, async {
        ep.pokes.send(relay, &RelayMsg::StunReq { tx }.encode()).await.ok()?;
        reply.await.ok()
    }).await.ok().flatten()
}

fn open_turn_route(
    ep: Arc<P2pEndpoint>, token: [u8; 16], relay: SocketAddr, peer: [u8; 32],
) -> (SocketAddr, RouteGuards) {
    let synth = ep.turn.lock().register(token, relay);
    tcp::start(&ep.turn, synth, token, relay, peer);
    // Re-sent every 4 s: a symmetric NAT drops an idle mapping and strands the relay's return path.
    let pokes = ep.pokes.clone();
    let alloc = RelayMsg::TurnAlloc { token }.encode();
    let keepalive = core().spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(4));
        loop {
            tick.tick().await;
            if pokes.send(relay, &alloc).await.is_err() {
                break;
            }
        }
    });
    (synth, (AbortGuard(keepalive), TurnGuard(ep, token)))
}

/// quinn sends to the synth for the connection's whole life, so the route outlives the session.
fn hold_route_while_open(
    ep: Arc<P2pEndpoint>, conn: Connection, guards: Vec<RouteGuards>, tasks: Vec<AbortGuard>,
) {
    core().spawn(async move {
        conn.closed().await;
        diagnostics::record(if ep.turn.lock().is_direct(conn.remote_address()) {
            Event::DirectLost
        } else { Event::RelayLost });
        drop(guards);
        drop(tasks);
    });
}

fn expect_inbound(
    ep: Arc<P2pEndpoint>, addrs: Vec<SocketAddr>,
) -> (mpsc::UnboundedReceiver<quinn::Incoming>, InboundGuard) {
    let (tx, rx) = mpsc::unbounded_channel();
    ep.inbound.lock().claim(&addrs, tx.clone());
    (rx, InboundGuard { ep, addrs, tx })
}

async fn punch_upgrade(
    ep: Arc<P2pEndpoint>, mut poke_rx: mpsc::Receiver<Poke>, key: DiscoKey,
    cands: Vec<SocketAddr>, token: [u8; 16], peer: [u8; 32],
) {
    let accept_from = |src| {
        ep.turn.lock().accept_from(&token, src);
    };
    match punch::punch(&ep.pokes, &mut poke_rx, key, cands, PUNCH_TIMEOUT, accept_from).await {
        Some(addr) if ep.turn.lock().set_direct(&token, addr) => {
            diagnostics::record(Event::DirectReady);
            log::info!("P2P[{}]: upgraded to direct {}", hex::encode(&peer[..4]), addr_short(addr));
        },
        Some(_) => {},
        None => {
            diagnostics::record(Event::PunchTimeout);
            log::debug!("P2P[{}]: no direct path — staying relayed", hex::encode(&peer[..4]));
        },
    }
}

pub(super) async fn connect_inner(
    ep: Arc<P2pEndpoint>, generation: u64,
    peer: [u8; 32], disclosure: Disclosure, answering: Option<[u8; 16]>,
) -> Result<PeerLink> {
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("no identity"))?;
    let chan = channel_for(&our_ipk, &peer);

    // Register the poke route and offer listener before announcing, so nothing races ahead.
    let (poke_tx, poke_rx) = mpsc::channel(64);
    ep.sessions.lock().insert(chan, poke_tx.clone());
    let poke_guard = PokeGuard { ep: ep.clone(), chan, tx: poke_tx };
    let (offers, offer_tx) = signal::listen(peer);
    let offer_guard = OfferGuard { peer, tx: offer_tx };

    let session = Session {
        ep: ep.clone(),
        peer,
        chan,
        id: rand_bytes::<16>(),
        answering,
        disclosure,
        poke_rx,
        poke_guard,
        offers,
        offer_guard,
    };
    let result = session.run().await;

    let (link, route) = match result {
        Ok(v) => v,
        Err(e) => {
            log::warn!("P2P[{}]: connection failed — {e}", hex::encode(&peer[..4]));
            return Err(e);
        },
    };
    // Until published, the connection is this attempt's: a cancelled verify closes it.
    struct Unpublished(Option<Connection>);
    impl Drop for Unpublished {
        fn drop(&mut self) {
            if let Some(conn) = self.0.take() { conn.close(0u32.into(), b"setup cancelled"); }
        }
    }
    let mut pending = Unpublished(Some(link.conn.clone()));
    if let Err(e) = timeout(VERIFY_TIMEOUT, link.verify_roundtrip()).await
        .map_err(|_| anyhow!("peer verification timed out")).and_then(|r| r)
    {
        diagnostics::record(Event::VerificationFailed);
        log::warn!("P2P[{}]: link verify failed — {e}", hex::encode(&peer[..4]));
        link.conn.close(0u32.into(), b"verify failed");
        return Err(e);
    }
    log::info!(
        "P2P[{}]: connected via {route} — {}",
        hex::encode(&peer[..4]),
        addr_short(link.remote_address())
    );
    if !link.still_permitted() {
        link.conn.close(0u32.into(), b"consent revoked");
        bail!("consent: revoked during connection setup");
    }
    ep.turn.lock().established(link.remote_address());
    ep.links.lock().publish(generation, link.clone())?;
    pending.0 = None;
    diagnostics::record(Event::LinkReady);
    let closed = link.conn.clone();
    core().spawn(async move {
        closed.closed().await;
        // An old connection closing must never evict its replacement.
        ep.links.lock().remove_closed(&peer, closed.stable_id());
    });
    // Both ends serve pulls, so a file offered in either direction is fetchable over this link.
    core().spawn(crate::transfer::serve_link(link.clone()));
    crate::transfer::on_link_ready(peer);
    Ok(link)
}

struct Session {
    ep:          Arc<P2pEndpoint>,
    peer:        [u8; 32],
    chan:        [u8; 8],
    id:          [u8; 16],
    answering:   Option<[u8; 16]>,
    disclosure:  Disclosure,
    poke_rx:     mpsc::Receiver<Poke>,
    poke_guard:  PokeGuard,
    offers:      mpsc::UnboundedReceiver<Offer>,
    offer_guard: OfferGuard,
}

impl Session {
    fn short(&self) -> String {
        hex::encode(&self.peer[..4])
    }

    async fn next_offer(&mut self) -> Option<Offer> {
        let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
        loop {
            let offer = tokio::time::timeout_at(deadline, self.offers.recv()).await.ok()??;
            if offer.matches(self.id, self.answering, now_ms()) {
                return Some(offer);
            }
            log::debug!("P2P[{}]: skipping an offer for another attempt", self.short());
        }
    }

    async fn run(mut self) -> Result<(PeerLink, &'static str)> {
        let ep = self.ep.clone();
        let peer = self.peer;
        let disclosure = self.disclosure;
        let direct = disclosure == Disclosure::Direct;
        if !direct { diagnostics::record(Event::RelayOnlyPolicy); }
        let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("no identity"))?;

        // The bridge and disco key in use are the dialer's, so the dialer needs nothing back
        // before it connects. A relay-only session publishes no candidates.
        let reflexive = if direct { refresh_reflexive(ep.clone()).await } else { None };
        let our_relay = assist_relay();
        let my_token = rand_bytes::<16>();
        let my_disco_key = rand_bytes::<32>();
        let cands = if direct {
            let mut c = candidate::local_candidates(ep.port);
            c.extend(reflexive);
            c
        } else {
            Vec::new()
        };
        let mine = Offer {
            session:       self.id,
            in_reply_to:   self.answering,
            expires_at_ms: now_ms() + OFFER_TTL_MS,
            candidates:    cands,
            relay:         our_relay,
            token:         my_token,
            disco_key:     my_disco_key,
        };
        if let Err(e) = timeout(SIGNAL_TIMEOUT, signal::send_offer(peer, &mine)).await
            .map_err(|_| anyhow!("sending peer offer timed out")).and_then(|r| r)
        {
            diagnostics::record(Event::SignalingFailed);
            return Err(e);
        }

        let dialer = our_ipk < peer;

        if dialer && let Some(tr) = our_relay {
            log::info!("P2P[{}]: dialer, connecting via relay bridge", self.short());
            let (synth, guards) = open_turn_route(ep.clone(), my_token, tr, peer);
            let key = DiscoKey::new(&my_disco_key, self.chan);
            let Session { poke_rx, poke_guard, offer_guard, mut offers, id, answering, .. } = self;
            let short = hex::encode(&peer[..4]);
            let upgrade_ep = ep.clone();
            let upgrade = AbortGuard(core().spawn(async move {
                let _guards = (poke_guard, offer_guard);
                let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
                let offer = loop {
                    let Ok(Some(offer)) = tokio::time::timeout_at(deadline, offers.recv()).await
                    else {
                        log::debug!("P2P[{short}]: no peer offer — staying relayed");
                        return;
                    };
                    if !offer.matches(id, answering, now_ms()) {
                        continue;
                    }
                    // The peer started its own attempt without seeing ours; a reply to it lets
                    // them join our bridge.
                    if offer.in_reply_to.is_none() && answering.is_none() {
                        let reply = Offer { in_reply_to: Some(offer.session), ..mine.clone() };
                        if let Err(e) = timeout(SIGNAL_TIMEOUT, signal::send_offer(peer, &reply)).await
                            .map_err(|_| anyhow!("crossing peer offer timed out")).and_then(|r| r) {
                            log::debug!("P2P[{short}]: reply to a crossing offer failed — {e}");
                        }
                    }
                    break offer;
                };
                if !direct || offer.candidates.is_empty() {
                    if direct { diagnostics::record(Event::MissingCandidates); }
                    return;
                }
                log::info!(
                    "P2P[{short}]: peer offers [{}], punching in background",
                    addrs_short(&offer.candidates)
                );
                punch_upgrade(upgrade_ep, poke_rx, key, offer.candidates, my_token, peer).await;
            }));
            let conn = timeout(DIAL_TIMEOUT, ep.endpoint.connect(synth, PEER_SNI)?)
                .await
                .map_err(|_| anyhow!("bridge dial timed out after {}s", DIAL_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(Into::into))
                .inspect_err(|_| diagnostics::record(Event::HandshakeFailed))?;
            hold_route_while_open(ep.clone(), conn.clone(), vec![guards], vec![upgrade]);
            return Ok((PeerLink { conn, dialer: true, disclosure, ipk: peer }, "relay"));
        }

        // Both remaining roles need the peer's offer: punch targets, or the dialer's secrets.
        let offer = self.next_offer().await.ok_or_else(|| {
            diagnostics::record(Event::SignalingFailed);
            anyhow!("timed out waiting for peer candidates")
        })?;
        log::info!(
            "P2P[{}]: {}, peer offers [{}]",
            self.short(),
            if dialer { "dialer (no relay)" } else { "acceptor" },
            addrs_short(&offer.candidates)
        );

        if dialer {
            anyhow::ensure!(direct, "relay required for this peer");
            let key = DiscoKey::new(&my_disco_key, self.chan);
            let addr =
                // No bridge, so heard sources need no relabel.
                punch::punch(
                    &ep.pokes,
                    &mut self.poke_rx,
                    key,
                    offer.candidates,
                    PUNCH_TIMEOUT,
                    |_| {},
                )
                .await
                .ok_or_else(|| {
                    diagnostics::record(Event::PunchTimeout);
                    anyhow!("no relay and no direct path")
                })?;
            log::info!("P2P[{}]: hole punched, dialing {}", self.short(), addr_short(addr));
            let conn = timeout(DIAL_TIMEOUT, ep.endpoint.connect(addr, PEER_SNI)?)
                .await
                .map_err(|_| anyhow!("direct dial timed out after {}s", DIAL_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(Into::into))
                .inspect_err(|_| diagnostics::record(Event::HandshakeFailed))?;
            diagnostics::record(Event::DirectReady);
            hold_route_while_open(ep.clone(), conn.clone(), vec![], vec![]);
            return Ok((PeerLink { conn, dialer: true, disclosure, ipk: peer }, "direct"));
        }

        let key = DiscoKey::new(&offer.disco_key, self.chan);
        let token = offer.token;
        let mut routes: Vec<RouteGuards> = Vec::new();
        let mut bridged_tokens = HashSet::new();
        let mut sources: Vec<SocketAddr> = Vec::new();
        match offer.relay {
            // Only the holder of the MLS-carried token can produce its synthetic source address.
            Some(tr) => {
                let (synth, guards) = open_turn_route(ep.clone(), token, tr, peer);
                routes.push(guards);
                bridged_tokens.insert(token);
                sources.push(synth);
            },
            // Direct, the dialer arrives from one of its own candidates.
            None if direct => sources.extend(offer.candidates.iter().copied()),
            None => bail!("relay required for this peer"),
        }
        if sources.is_empty() {
            diagnostics::record(Event::MissingCandidates);
            bail!("peer offers neither a relay nor candidates");
        }
        let (mut inbound, mut inbound_guard) = expect_inbound(ep.clone(), sources);

        let Session { poke_rx, poke_guard, mut offers, offer_guard, id, answering, .. } = self;
        let short = hex::encode(&peer[..4]);
        let peer_cands = offer.candidates.clone();
        let upgrade_ep = ep.clone();
        let upgrade = AbortGuard(core().spawn(async move {
            let _poke_guard = poke_guard;
            if direct && !peer_cands.is_empty() {
                punch_upgrade(upgrade_ep, poke_rx, key, peer_cands, token, peer).await;
            }
        }));
        log::info!("P2P[{short}]: acceptor waiting for inbound");
        let deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
        let incoming = loop {
            tokio::select! {
                got = inbound.recv() => {
                    break got.ok_or_else(|| anyhow!("endpoint closed"))?;
                },
                // A dialer retry uses a fresh token, so bridge that one too.
                got = offers.recv() => {
                    let Some(again) = got else { continue };
                    if !again.matches(id, answering, now_ms()) || again.token == token {
                        continue;
                    }
                    // Repeated retries may bridge only a few distinct tokens.
                    if let Some(tr) = again.relay
                        && bridged_tokens.len() < 3 && bridged_tokens.insert(again.token)
                    {
                        let (synth, guards) = open_turn_route(ep.clone(), again.token, tr, peer);
                        routes.push(guards);
                        ep.inbound.lock().claim(&[synth], inbound_guard.tx.clone());
                        inbound_guard.addrs.push(synth);
                        log::info!("P2P[{short}]: dialer retried, bridging its new token too");
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timed out waiting for inbound connection");
                },
            }
        };
        drop(offer_guard);
        // Bound the handshake too: past the dialer's own timeout nobody drives it, and quinn's
        // idle timer would hold this peer's `connecting` slot for another half minute.
        let conn = timeout(ACCEPT_TIMEOUT, incoming.accept()?)
            .await
            .map_err(|_| {
                anyhow!("inbound handshake timed out after {}s", ACCEPT_TIMEOUT.as_secs())
            })??;
        drop(inbound_guard);
        hold_route_while_open(ep.clone(), conn.clone(), routes, vec![upgrade]);
        Ok((PeerLink { conn, dialer: false, disclosure, ipk: peer }, "inbound"))
    }
}
