//! Presence + last-seen (same-relay MVP).
//!
//! The relay is the presence authority — it already holds the connected-client
//! map, so it needn't be told who is online. A client `SubscribePresence`s with
//! its contact set; the relay replies with a snapshot and thereafter pushes
//! single-entry deltas as those contacts connect/disconnect.
//!
//! Authorization is **mutual**: A learns B's presence only when A subscribed to
//! B *and* B subscribed to A. `Relay::presence_subs` is both lists at once.
//!
//! MVP scope: same-relay only. A contact connected to a different relay looks
//! offline here — cross-relay fan-out is a follow-up.

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::PresenceP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::types::bytes::Bytes;
use quinn::Connection;
use std::collections::HashSet;

use crate::quic::handler::client::ClientCtxHandle;
use crate::relay::RelayRef;
use crate::util::systime;

/// Handle a `SubscribePresence`: record interest, snapshot the caller's mutual
/// contacts back to it, and announce the caller's own online-ness to those of
/// them that are connected here.
pub(super) async fn handle_subscribe(sub: SubscribePresenceP, ctx: ClientCtxHandle) -> Result<()> {
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let contacts: HashSet<[u8; 32]> = sub.contacts.iter().map(|b| b.0).collect();

    relay.presence_subs.write().insert(me, contacts.clone());

    // Snapshot: my view of each contact that also subscribed to me.
    let snapshot: Vec<PresenceP> = {
        let subs = relay.presence_subs.read();
        let clients = relay.clients.read();
        contacts
            .iter()
            .filter(|c| is_mutual(&subs, c, &me))
            .map(|c| PresenceP { who: Bytes(*c), last_seen: last_seen_of(relay, &clients, c) })
            .collect()
    };
    if !snapshot.is_empty() {
        push(&ctx.conn, snapshot).await;
    }

    // Announce me → my mutual online contacts (they now see me online).
    let targets = mutual_online_conns(relay, &contacts, &me);
    let me_online = vec![PresenceP { who: Bytes(me), last_seen: None }];
    for conn in targets {
        push(&conn, me_online.clone()).await;
    }
    Ok(())
}

/// On disconnect: persist last-seen and tell my mutual online contacts I'm gone.
/// Called from the client loop teardown *after* the clients-map eviction, so I
/// no longer appear online to myself.
pub(crate) async fn on_disconnect(relay: &RelayRef, me: &[u8; 32]) {
    let now = systime().as_millis() as u64;
    let _ = relay.store.put_last_seen(me, now);

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
    let offline = vec![PresenceP { who: Bytes(*me), last_seen: Some(now) }];
    for conn in targets {
        push(&conn, offline.clone()).await;
    }
}

/// `contact` and `me` each subscribed to the other. `me`'s side is the caller's
/// responsibility (it's iterating its own contact set); this checks `contact`'s.
fn is_mutual(subs: &std::collections::HashMap<[u8; 32], HashSet<[u8; 32]>>, contact: &[u8; 32], me: &[u8; 32]) -> bool {
    subs.get(contact).map(|s| s.contains(me)).unwrap_or(false)
}

/// `None` if the contact is connected here, else its stored last-seen
/// (`Some(0)` when we never recorded one).
fn last_seen_of(
    relay: &RelayRef, clients: &std::collections::HashMap<[u8; 32], Connection>, c: &[u8; 32],
) -> Option<u64> {
    if clients.contains_key(c) {
        None
    } else {
        Some(relay.store.get_last_seen(c).unwrap_or(0))
    }
}

/// Connections of `me`'s mutual contacts that are currently online here.
fn mutual_online_conns(relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32]) -> Vec<Connection> {
    let subs = relay.presence_subs.read();
    let clients = relay.clients.read();
    contacts
        .iter()
        .filter(|c| is_mutual(&subs, c, me))
        .filter_map(|c| clients.get(c).cloned())
        .collect()
}

/// Fire a presence push on a fresh uni-purpose bi-stream (no reply expected).
async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = SRelayPacket::Presence(entries).send(&mut tx).await;
        let _ = tx.finish();
    }
}
