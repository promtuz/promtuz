//! Presence + last-seen + idle (same-relay MVP).
//!
//! The relay is the presence authority — it holds the connected-client map, so
//! it derives Online/Offline itself. `Idle` it can't observe (a frozen app is
//! indistinguishable from a quiet one until the idle timeout), so the client
//! asserts it via `SetPresence`. A client `SubscribePresence`s with its contact
//! set; the relay replies with a snapshot and thereafter pushes single-entry
//! deltas as contacts connect / go idle / disconnect.
//!
//! Authorization is **mutual**: A learns B's presence only when A subscribed to
//! B *and* B subscribed to A. `Relay::presence_subs` is both lists at once.
//!
//! MVP scope: same-relay + plaintext. Cross-relay fan-out and the encrypted
//! privacy pass (beacons + blinded tokens) are follow-ups — see `PRESENCE.md`.

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::PresenceMode;
use common::proto::client_rel::PresenceP;
use common::proto::client_rel::PresenceState;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::proto::dht_p2p::RelayPresenceState;
use common::proto::dht_p2p::presence_state_signing_input;
use common::types::bytes::Bytes;
use quinn::Connection;

use crate::quic::handler::client::ClientCtxHandle;
use crate::relay::RelayRef;
use crate::util::systime;

/// Handle a `SubscribePresence`: record interest, snapshot the caller's mutual
/// contacts back to it, and announce the caller (now Online) to those of them
/// connected here.
pub(super) async fn handle_subscribe(sub: SubscribePresenceP, ctx: ClientCtxHandle) -> Result<()> {
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let contacts: HashSet<[u8; 32]> = sub.contacts.iter().map(|b| b.0).collect();

    let now = systime().as_millis() as u64;
    let Some(dht) = relay.dht.as_ref() else { return Ok(()) };
    if sub.lease.user.0 != me || sub.lease.relay_id != dht.node_id || !sub.lease.verify(now) {
        return Ok(());
    }
    for consent in &sub.consents {
        if consent.owner.0 != me || !consent.verify(now) {
            return Ok(());
        }
        let _ = relay.store.put_presence_consent(consent);
        tokio::spawn(crate::dht::forward::forward_presence_consent(dht.clone(), consent.clone()));
    }
    if contacts.iter().any(|contact| {
        !sub.consents.iter().any(|consent| consent.recipient.0 == *contact && consent.granted)
    }) {
        return Ok(());
    }
    if !relay.store.put_presence_lease(&sub.lease).unwrap_or(false) {
        return Ok(());
    }
    relay.presence_leases.write().insert(me, sub.lease.clone());
    tokio::spawn(crate::dht::forward::forward_presence_lease(dht.clone(), sub.lease));
    relay.presence_subs.write().insert(me, contacts.clone());

    let snapshot: Vec<PresenceP> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        let idle = relay.presence_mode.read();
        contacts
            .iter()
            .filter(|c| is_mutual(relay, &subs, c, &me))
            .map(|c| PresenceP { who: Bytes(*c), state: state_of(relay, &clients, &idle, &me, c) })
            .collect()
    };
    if !snapshot.is_empty() {
        push(&ctx.conn, snapshot).await;
    }

    // Subscribing = foregrounded, so announce Online.
    announce(relay, &contacts, &me, PresenceState::Online, systime().as_millis() as u64).await;
    Ok(())
}

/// Handle a `SetPresence`: update our idle flag and push the new state to our
/// mutual online contacts.
pub(super) async fn handle_set_presence(mode: PresenceMode, ctx: ClientCtxHandle) -> Result<()> {
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let state = match mode {
        PresenceMode::Idle => {
            let now = systime().as_millis() as u64;
            relay.presence_mode.write().insert(me, now);
            PresenceState::Idle { since: now }
        },
        PresenceMode::Active => {
            relay.presence_mode.write().remove(&me);
            PresenceState::Online
        },
    };
    let contacts = relay.presence_subs.read().get(&me).cloned().unwrap_or_default();
    announce(relay, &contacts, &me, state, systime().as_millis() as u64).await;
    Ok(())
}

/// On disconnect: persist last-seen, drop the idle flag, tell mutual online
/// contacts we're gone. Called after the clients-map eviction, so we no longer
/// read as online to ourselves.
pub(crate) async fn on_disconnect(relay: &RelayRef, me: &[u8; 32]) {
    let now = systime().as_millis() as u64;
    let _ = relay.store.put_last_seen(me, now);
    relay.presence_mode.write().remove(me);

    let my_contacts = relay.presence_subs.read().get(me).cloned().unwrap_or_default();
    let targets: Vec<Connection> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        my_contacts
            .iter()
            .filter(|c| is_mutual(relay, &subs, c, me))
            .filter_map(|c| clients.get(c).cloned())
            .collect()
    };
    let offline =
        vec![PresenceP { who: Bytes(*me), state: PresenceState::Offline { last_seen: now } }];
    for conn in targets {
        push(&conn, offline.clone()).await;
    }
    forward_to_homes(relay, &my_contacts, me, PresenceState::Offline { last_seen: now }, now);
}

/// Push our `state` (as `who = me`) to every mutual contact online here.
async fn announce(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32], state: PresenceState,
    observed_at_ms: u64,
) {
    let targets: Vec<Connection> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        contacts
            .iter()
            .filter(|c| is_mutual(relay, &subs, c, me))
            .filter_map(|c| clients.get(c).cloned())
            .collect()
    };
    let entry = vec![PresenceP { who: Bytes(*me), state: state.clone() }];
    for conn in targets {
        push(&conn, entry.clone()).await;
    }
    forward_to_homes(relay, contacts, me, state, observed_at_ms);
}

fn forward_to_homes(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32], state: PresenceState,
    observed_at_ms: u64,
) {
    let Some(dht) = relay.dht.as_ref().cloned() else { return };
    let Some(lease) = relay.presence_leases.read().get(me).cloned() else { return };
    let version = {
        let mut versions = relay.presence_versions.write();
        let next = versions.get(me).copied().unwrap_or(0).max(observed_at_ms).saturating_add(1);
        versions.insert(*me, next);
        next
    };
    for contact in contacts {
        if relay.store.has_presence_consent(me, contact) {
            let mut record = RelayPresenceState {
                recipient: (*contact).into(),
                who: (*me).into(),
                lease: lease.clone(),
                state: state.clone(),
                version,
                observed_at_ms,
                relay_pubkey: dht.signing_key.verifying_key().to_bytes().into(),
                relay_sig: [0; 64].into(),
            };
            use ed25519_dalek::Signer;
            record.relay_sig =
                dht.signing_key.sign(&presence_state_signing_input(&record)).to_bytes().into();
            tokio::spawn(crate::dht::forward::forward_presence_state(dht.clone(), record));
        }
    }
}

/// `contact` and `me` each subscribed to the other. `me`'s side is the caller's
/// responsibility (it iterates its own contact set); this checks `contact`'s.
fn is_mutual(
    relay: &RelayRef, subs: &HashMap<[u8; 32], HashSet<[u8; 32]>>, contact: &[u8; 32],
    me: &[u8; 32],
) -> bool {
    subs.get(contact)
        .map(|s| s.contains(me))
        .unwrap_or_else(|| relay.store.has_presence_consent(contact, me))
}

/// Derive a contact's state: connected → Idle{since} if it flagged idle, else
/// Online; not connected → Offline{last_seen} (0 = unknown).
fn state_of(
    relay: &RelayRef, clients: &HashMap<[u8; 32], Connection>, idle: &HashMap<[u8; 32], u64>,
    viewer: &[u8; 32], c: &[u8; 32],
) -> PresenceState {
    if clients.contains_key(c) {
        match idle.get(c) {
            Some(&since) => PresenceState::Idle { since },
            None => PresenceState::Online,
        }
    } else {
        relay.store.get_presence_state(viewer, c).unwrap_or(PresenceState::Offline {
            last_seen: relay.store.get_last_seen(c).unwrap_or(0),
        })
    }
}

/// Fire a presence push on a fresh bi-stream (no reply expected).
async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = SRelayPacket::Presence(entries).send(&mut tx).await;
        let _ = tx.finish();
    }
}
