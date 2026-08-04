//! Presence + last-seen + idle (same-relay MVP).
//!
//! The relay holds the connected-client map, but connection alone is NOT
//! presence — a background wake-drain is connected too. Online requires an
//! explicit foreground assertion (`SetPresence(Active)`); a connected client
//! that hasn't asserted reads as `Offline{last_seen}`. A client
//! `SubscribePresence`s with its contact set; the relay replies with a snapshot
//! and thereafter pushes single-entry deltas as contacts assert / background /
//! disconnect.
//!
//! Authorization is **mutual**: A learns B's presence only when A subscribed to
//! B *and* B subscribed to A. `Relay::presence_subs` is both lists at once.
//!
//! MVP scope: same-relay + plaintext. Cross-relay fan-out and the encrypted
//! privacy pass (beacons + blinded tokens) are follow-ups — see `PRESENCE.md`.

use std::collections::HashSet;
use std::time::Duration;

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
use tokio_util::sync::CancellationToken;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::STREAM_OPEN_TIMEOUT;
use crate::quic::handler::client::events::bounded_fanout;
use crate::quic::handler::client::events::spawn_tied;
use crate::relay::RelayRef;
use crate::util::systime;

const MAX_PRESENCE_CONTACTS: usize = 256;
const MAX_PRESENCE_CONSENTS: usize = 256;
const PRESENCE_FANOUT_CONCURRENCY: usize = 8;
/// Wall-clock ceiling for one announce or one home fan-out, whatever the
/// contact count. Both are amplifiers driven by a single client packet.
const PRESENCE_FANOUT_BUDGET: Duration = Duration::from_secs(5);

/// Handle a `SubscribePresence`: record interest, snapshot the caller's mutual
/// contacts back to it, and announce the caller (now Online) to those of them
/// connected here.
pub(super) async fn handle_subscribe(sub: SubscribePresenceP, ctx: ClientCtxHandle) -> Result<()> {
    if sub.contacts.len() > MAX_PRESENCE_CONTACTS || sub.consents.len() > MAX_PRESENCE_CONSENTS {
        return Ok(());
    }
    if ctx.limits.subscribe_presence.check().is_err() {
        return Ok(());
    }

    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = systime().as_millis() as u64;
    let Some(dht) = relay.dht.as_ref().cloned() else { return Ok(()) };

    if sub.lease.user.0 != me || sub.lease.relay_id != dht.node_id || !sub.lease.verify(now) {
        return Ok(());
    }
    if sub.consents.iter().any(|consent| consent.owner.0 != me || !consent.verify(now)) {
        return Ok(());
    }
    let contacts: HashSet<[u8; 32]> = sub.contacts.iter().map(|b| b.0).collect();
    if contacts.iter().any(|contact| {
        !sub.consents.iter().any(|consent| consent.recipient.0 == *contact && consent.granted)
    }) {
        return Ok(());
    }

    let store = relay.store.clone();
    let consents = sub.consents;
    let lease = sub.lease;
    let (consents, lease, lease_stored) = tokio::task::spawn_blocking(move || {
        for consent in &consents {
            let _ = store.put_presence_consent(consent);
        }
        let stored = store.put_presence_lease(&lease).unwrap_or(false);
        (consents, lease, stored)
    })
    .await?;
    if !lease_stored {
        return Ok(());
    }

    relay.presence_leases.write().insert(me, lease.clone());
    relay.presence_subs.write().insert(me, contacts.clone());

    spawn_tied(&ctx.cancel, {
        let dht = dht.clone();
        async move {
            crate::dht::forward::forward_presence_lease(dht.clone(), lease).await;
            let fanout = bounded_fanout(
                consents
                    .into_iter()
                    .map(|c| crate::dht::forward::forward_presence_consent(dht.clone(), c))
                    .collect(),
                PRESENCE_FANOUT_CONCURRENCY,
            );
            let _ = tokio::time::timeout(PRESENCE_FANOUT_BUDGET, fanout).await;
        }
    });

    let mutual = mutual_contacts(relay, &contacts, &me);
    let snapshot: Vec<PresenceP> = {
        let online: HashSet<[u8; 32]> = {
            let clients = relay.clients.read();
            let active = relay.active_clients.read();
            mutual
                .iter()
                .copied()
                .filter(|c| clients.contains_key(c) && active.contains_key(c))
                .collect()
        };
        mutual
            .iter()
            .map(|c| PresenceP {
                who:   Bytes(*c),
                state: if online.contains(c) {
                    PresenceState::Online
                } else {
                    stored_state(relay, &me, c)
                },
            })
            .collect()
    };
    if !snapshot.is_empty() {
        push(&ctx.conn, snapshot).await;
    }

    // Announce our ACTUAL state: connection alone is not presence, so a
    // background wake-drain re-subscribe reads Offline until it asserts Active.
    let state = match relay.active_clients.read().get(&me) {
        Some(_) => PresenceState::Online,
        None => PresenceState::Offline { last_seen: relay.store.get_last_seen(&me).unwrap_or(0) },
    };
    announce(relay, &contacts, &me, state, systime().as_millis() as u64, &ctx.cancel).await;
    Ok(())
}

/// Handle a `SetPresence`: update our foreground-active flag and push the new
/// state to our mutual online contacts.
pub(super) async fn handle_set_presence(mode: PresenceMode, ctx: ClientCtxHandle) -> Result<()> {
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = systime().as_millis() as u64;
    let state = match mode {
        PresenceMode::Active => {
            relay.active_clients.write().insert(me, now);
            PresenceState::Online
        },
        // Idle = backgrounded / not foreground. Only a device that was actually
        // foreground-Active counts as "seen now": a background wake (reverse-wake,
        // push-drain, reconnect) asserts Idle without ever going Active, so it
        // reports its real prior last-seen instead of stamping now.
        PresenceMode::Idle => {
            let was_active = relay.active_clients.write().remove(&me).is_some();
            let last_seen = if was_active {
                let _ = relay.store.put_last_seen(&me, now);
                now
            } else {
                relay.store.get_last_seen(&me).unwrap_or(0)
            };
            PresenceState::Offline { last_seen }
        },
    };
    // The local flag above is O(1) and always applied; only the fan-out that a
    // toggle triggers is rate-limited.
    if ctx.limits.set_presence.check().is_err() {
        return Ok(());
    }
    let contacts = relay.presence_subs.read().get(&me).cloned().unwrap_or_default();
    announce(relay, &contacts, &me, state, systime().as_millis() as u64, &ctx.cancel).await;
    Ok(())
}

/// On disconnect: drop the active flag, stamp last-seen only if we were
/// foreground-active (a background connection keeps its real prior last-seen),
/// and tell mutual online contacts we're gone. Called after the clients-map
/// eviction, so we no longer read as online to ourselves.
pub(crate) async fn on_disconnect(
    relay: &RelayRef, me: &[u8; 32], cancel: &CancellationToken,
) {
    let now = systime().as_millis() as u64;
    let was_active = relay.active_clients.write().remove(me).is_some();
    let last_seen = if was_active {
        let _ = relay.store.put_last_seen(me, now);
        now
    } else {
        relay.store.get_last_seen(me).unwrap_or(0)
    };

    let my_contacts = relay.presence_subs.read().get(me).cloned().unwrap_or_default();
    let state = PresenceState::Offline { last_seen };
    announce(relay, &my_contacts, me, state, now, cancel).await;
}

/// Push our `state` (as `who = me`) to every mutual contact online here.
async fn announce(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32], state: PresenceState,
    observed_at_ms: u64, cancel: &CancellationToken,
) {
    let mutual = mutual_contacts(relay, contacts, me);
    let targets: Vec<Connection> = {
        let clients = relay.clients.read();
        mutual.iter().filter_map(|c| clients.get(c).cloned()).collect()
    };
    let entry = vec![PresenceP { who: Bytes(*me), state: state.clone() }];
    let pushes = bounded_fanout(
        targets
            .into_iter()
            .map(|conn| {
                let entry = entry.clone();
                async move { push(&conn, entry).await }
            })
            .collect(),
        PRESENCE_FANOUT_CONCURRENCY,
    );
    let _ = tokio::time::timeout(PRESENCE_FANOUT_BUDGET, pushes).await;

    forward_to_homes(relay, contacts, me, state, observed_at_ms, cancel);
}

fn forward_to_homes(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32], state: PresenceState,
    observed_at_ms: u64, cancel: &CancellationToken,
) {
    let Some(dht) = relay.dht.as_ref().cloned() else { return };
    let Some(lease) = relay.presence_leases.read().get(me).cloned() else { return };
    let version = {
        let mut versions = relay.presence_versions.write();
        let next = versions.get(me).copied().unwrap_or(0).max(observed_at_ms).saturating_add(1);
        versions.insert(*me, next);
        next
    };
    let targets: Vec<[u8; 32]> = contacts.iter().copied().take(MAX_PRESENCE_CONTACTS).collect();
    let store = relay.store.clone();
    let me = *me;

    spawn_tied(cancel, async move {
        let signing_key = dht.signing_key.clone();
        let relay_pubkey = signing_key.verifying_key().to_bytes();
        let records = tokio::task::spawn_blocking(move || {
            use ed25519_dalek::Signer;
            targets
                .into_iter()
                .filter(|contact| store.has_presence_consent(&me, contact))
                .map(|contact| {
                    let mut record = RelayPresenceState {
                        recipient: contact.into(),
                        who: me.into(),
                        lease: lease.clone(),
                        state: state.clone(),
                        version,
                        observed_at_ms,
                        relay_pubkey: relay_pubkey.into(),
                        relay_sig: [0; 64].into(),
                    };
                    record.relay_sig =
                        signing_key.sign(&presence_state_signing_input(&record)).to_bytes().into();
                    record
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let fanout = bounded_fanout(
            records
                .into_iter()
                .map(|record| crate::dht::forward::forward_presence_state(dht.clone(), record))
                .collect(),
            PRESENCE_FANOUT_CONCURRENCY,
        );
        let _ = tokio::time::timeout(PRESENCE_FANOUT_BUDGET, fanout).await;
    });
}

/// Contacts that also subscribed to `me`. Connected contacts are answered from
/// `presence_subs`; the rest fall back to stored consent, which is a disk read
/// and so runs only after the map guard is released.
fn mutual_contacts(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32],
) -> Vec<[u8; 32]> {
    let (mut mutual, unsubscribed) = {
        let subs = relay.presence_subs.read();
        let mut mutual: Vec<[u8; 32]> = Vec::new();
        let mut unsubscribed: Vec<[u8; 32]> = Vec::new();
        for contact in contacts {
            match subs.get(contact) {
                Some(theirs) if theirs.contains(me) => mutual.push(*contact),
                Some(_) => {},
                None => unsubscribed.push(*contact),
            }
        }
        (mutual, unsubscribed)
    };
    mutual.extend(
        unsubscribed.into_iter().filter(|contact| relay.store.has_presence_consent(contact, me)),
    );
    mutual
}

/// A contact's last durable state, `Offline{last_seen}` (0 = unknown) when we
/// have never recorded one.
fn stored_state(relay: &RelayRef, viewer: &[u8; 32], contact: &[u8; 32]) -> PresenceState {
    relay.store.get_presence_state(viewer, contact).unwrap_or(PresenceState::Offline {
        last_seen: relay.store.get_last_seen(contact).unwrap_or(0),
    })
}

/// Fire a presence push on a fresh bi-stream (no reply expected).
async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    let _ = tokio::time::timeout(STREAM_OPEN_TIMEOUT, async {
        let (mut tx, _rx) = conn.open_bi().await.ok()?;
        SRelayPacket::Presence(entries).send(&mut tx).await.ok()?;
        tx.finish().ok()
    })
    .await;
}
