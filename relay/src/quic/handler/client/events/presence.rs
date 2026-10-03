//! Presence subscriptions and announcements. Authorization is mutual: A sees B only when B has
//! also subscribed to, or consented to, A.

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
use common::utils::now_ms;
use quinn::Connection;
use tokio_util::sync::CancellationToken;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::STREAM_OPEN_TIMEOUT;
use crate::quic::handler::client::events::bounded_fanout;
use crate::quic::handler::client::events::spawn_tied;
use crate::relay::RelayRef;

const MAX_PRESENCE_CONTACTS: usize = 256;
const MAX_PRESENCE_CONSENTS: usize = 256;
const PRESENCE_FANOUT_CONCURRENCY: usize = 8;
/// Wall-clock ceiling for one announce or one home fan-out, whatever the
/// contact count. Both are amplifiers driven by a single client packet.
const PRESENCE_FANOUT_BUDGET: Duration = Duration::from_secs(5);

pub(super) async fn handle_subscribe(sub: SubscribePresenceP, ctx: ClientCtxHandle) -> Result<()> {
    if sub.contacts.len() > MAX_PRESENCE_CONTACTS || sub.consents.len() > MAX_PRESENCE_CONSENTS {
        return Ok(());
    }
    if ctx.limits.subscribe_presence.check().is_err() {
        return Ok(());
    }

    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = now_ms();
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

    // Connection alone is not presence: a background wake-drain re-subscribe reads Offline until
    // it asserts Active.
    let state = match relay.active_clients.read().get(&me) {
        Some(_) => PresenceState::Online,
        None => PresenceState::Offline { last_seen: relay.store.get_last_seen(&me).unwrap_or(0) },
    };
    announce(relay, &contacts, &me, state, now_ms(), &ctx.cancel).await;
    Ok(())
}

pub(super) async fn handle_set_presence(mode: PresenceMode, ctx: ClientCtxHandle) -> Result<()> {
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = now_ms();
    let state = match mode {
        PresenceMode::Active => {
            relay.active_clients.write().insert(me, now);
            PresenceState::Online
        },
        // Only a device that was foreground-Active counts as seen now. A background wake asserts
        // Idle without going Active, so it keeps its prior last-seen.
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
    // The flag above always applies; only the fan-out is rate-limited.
    if ctx.limits.set_presence.check().is_err() {
        return Ok(());
    }
    let contacts = relay.presence_subs.read().get(&me).cloned().unwrap_or_default();
    announce(relay, &contacts, &me, state, now_ms(), &ctx.cancel).await;
    Ok(())
}

pub(crate) async fn on_disconnect(
    relay: &RelayRef, me: &[u8; 32], cancel: &CancellationToken,
) {
    let now = now_ms();
    let was_active = relay.active_clients.write().remove(me).is_some();
    let last_seen = if was_active {
        let _ = relay.store.put_last_seen(me, now);
        now
    } else {
        relay.store.get_last_seen(me).unwrap_or(0)
    };

    let my_contacts = relay.presence_subs.read().get(me).cloned().unwrap_or_default();
    let lease = relay.presence_leases.read().get(me).cloned();
    let state = PresenceState::Offline { last_seen };
    announce(relay, &my_contacts, me, state, now, cancel).await;

    relay.presence_versions.write().remove(me);
    // A session that subscribed during the announce holds a newer lease.
    let mut leases = relay.presence_leases.write();
    if leases.get(me) == lease.as_ref() {
        leases.remove(me);
        relay.presence_subs.write().remove(me);
    }
}

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

/// Contacts that also subscribed to `me`. Subscribers answer from `presence_subs`; the rest fall
/// back to stored consent, a disk read that runs after the map guard is released.
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

fn stored_state(relay: &RelayRef, viewer: &[u8; 32], contact: &[u8; 32]) -> PresenceState {
    relay.store.get_presence_state(viewer, contact).unwrap_or(PresenceState::Offline {
        last_seen: relay.store.get_last_seen(contact).unwrap_or(0),
    })
}

async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    let _ = tokio::time::timeout(STREAM_OPEN_TIMEOUT, async {
        let (mut tx, _rx) = conn.open_bi().await.ok()?;
        SRelayPacket::Presence(entries).send(&mut tx).await.ok()?;
        tx.finish().ok()
    })
    .await;
}
