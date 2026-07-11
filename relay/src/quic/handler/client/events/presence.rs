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

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::PresenceMode;
use common::proto::client_rel::PresenceP;
use common::proto::client_rel::PresenceState;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::types::bytes::Bytes;
use quinn::Connection;
use std::collections::HashMap;
use std::collections::HashSet;

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

    relay.presence_subs.write().insert(me, contacts.clone());

    let snapshot: Vec<PresenceP> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        let idle = relay.presence_mode.read();
        contacts
            .iter()
            .filter(|c| is_mutual(&subs, c, &me))
            .map(|c| PresenceP { who: Bytes(*c), state: state_of(relay, &clients, &idle, c) })
            .collect()
    };
    if !snapshot.is_empty() {
        push(&ctx.conn, snapshot).await;
    }

    // Subscribing = foregrounded, so announce Online.
    announce(relay, &contacts, &me, PresenceState::Online).await;
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
    announce(relay, &contacts, &me, state).await;
    Ok(())
}

/// On disconnect: persist last-seen, drop the idle flag, tell mutual online
/// contacts we're gone. Called after the clients-map eviction, so we no longer
/// read as online to ourselves.
pub(crate) async fn on_disconnect(relay: &RelayRef, me: &[u8; 32]) {
    let now = systime().as_millis() as u64;
    let _ = relay.store.put_last_seen(me, now);
    relay.presence_mode.write().remove(me);

    let targets: Vec<Connection> = {
        let mut subs = relay.presence_subs.write();
        let Some(my_contacts) = subs.remove(me) else { return };
        let clients = relay.clients.read();
        my_contacts
            .iter()
            .filter(|c| is_mutual(&subs, c, me))
            .filter_map(|c| clients.get(c).cloned())
            .collect()
    };
    let offline = vec![PresenceP { who: Bytes(*me), state: PresenceState::Offline { last_seen: now } }];
    for conn in targets {
        push(&conn, offline.clone()).await;
    }
}

/// Push our `state` (as `who = me`) to every mutual contact online here.
async fn announce(relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32], state: PresenceState) {
    let targets: Vec<Connection> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        contacts
            .iter()
            .filter(|c| is_mutual(&subs, c, me))
            .filter_map(|c| clients.get(c).cloned())
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let entry = vec![PresenceP { who: Bytes(*me), state }];
    for conn in targets {
        push(&conn, entry.clone()).await;
    }
}

/// `contact` and `me` each subscribed to the other. `me`'s side is the caller's
/// responsibility (it iterates its own contact set); this checks `contact`'s.
fn is_mutual(subs: &HashMap<[u8; 32], HashSet<[u8; 32]>>, contact: &[u8; 32], me: &[u8; 32]) -> bool {
    subs.get(contact).map(|s| s.contains(me)).unwrap_or(false)
}

/// Derive a contact's state: connected → Idle{since} if it flagged idle, else
/// Online; not connected → Offline{last_seen} (0 = unknown).
fn state_of(
    relay: &RelayRef, clients: &HashMap<[u8; 32], Connection>, idle: &HashMap<[u8; 32], u64>,
    c: &[u8; 32],
) -> PresenceState {
    if clients.contains_key(c) {
        match idle.get(c) {
            Some(&since) => PresenceState::Idle { since },
            None => PresenceState::Online,
        }
    } else {
        PresenceState::Offline { last_seen: relay.store.get_last_seen(c).unwrap_or(0) }
    }
}

/// Fire a presence push on a fresh bi-stream (no reply expected).
async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = SRelayPacket::Presence(entries).send(&mut tx).await;
        let _ = tx.finish();
    }
}
