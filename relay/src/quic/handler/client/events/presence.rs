//! Presence interests and signed publication consent are independent. Local observations and
//! policy changes reach durable storage before acknowledgement or notification.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::PresenceMode;
use common::proto::client_rel::PresenceP;
use common::proto::client_rel::PresenceState;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::proto::client_rel::{MAX_PRESENCE_CONSENTS, MAX_PRESENCE_CONTACTS};
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

const PRESENCE_FANOUT_CONCURRENCY: usize = 8;
/// Wall-clock ceiling for one announce or one home fan-out, whatever the
/// contact count. Both are amplifiers driven by a single client packet.
const PRESENCE_FANOUT_BUDGET: Duration = Duration::from_secs(5);

pub(super) async fn handle_subscribe(
    sub: SubscribePresenceP, ctx: ClientCtxHandle,
) -> Result<bool> {
    let _update = ctx.relay.presence_update(&ctx.ipk.to_bytes()).await;
    if sub.contacts.len() > MAX_PRESENCE_CONTACTS || sub.consents.len() > MAX_PRESENCE_CONSENTS {
        return Ok(false);
    }
    if ctx.limits.subscribe_presence.check().is_err() {
        return Ok(false);
    }

    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = now_ms();
    if !is_current(&ctx)
        || sub.lease.user.0 != me
        || sub.lease.relay_id != relay.node_id
        || !sub.lease.verify(now)
    {
        return Ok(false);
    }
    let mut recipients = HashSet::new();
    if sub.consents.iter().any(|consent| {
        consent.owner.0 != me || !consent.verify(now) || !recipients.insert(consent.recipient.0)
    }) {
        return Ok(false);
    }
    let contacts: HashSet<[u8; 32]> = sub.contacts.iter().map(|b| b.0).collect();
    let revoked: Vec<_> =
        sub.consents.iter().filter(|c| !c.granted).map(|c| c.recipient.0).collect();
    let store = relay.store.clone();
    let stored_sub = sub.clone();
    if !tokio::task::spawn_blocking(move || store.put_presence_subscription(&stored_sub)).await?? {
        return Ok(false);
    }
    relay.store.persist_barrier().wait().await?;
    {
        let clients = relay.clients.read();
        if !clients.get(&me).is_some_and(|conn| conn.stable_id() == ctx.conn.stable_id()) {
            return Ok(false);
        }
        relay.presence_leases.write().insert(me, sub.lease.clone());
        relay.presence_subs.write().insert(me, contacts.clone());
    }

    let revoked_targets: Vec<_> = {
        let clients = relay.clients.read();
        revoked.iter().filter_map(|viewer| clients.get(viewer).cloned()).collect()
    };
    let clears = bounded_fanout(
        revoked_targets
            .into_iter()
            .map(|conn| async move {
                push(
                    &conn,
                    vec![PresenceP {
                        who: me.into(),
                        state: PresenceState::Offline { last_seen: 0 },
                    }],
                )
                .await;
            })
            .collect(),
        PRESENCE_FANOUT_CONCURRENCY,
    );
    let _ = tokio::time::timeout(PRESENCE_FANOUT_BUDGET, clears).await;

    if let Some(dht) = relay.dht.as_ref().cloned() {
        spawn_tied(&ctx.cancel, async move {
            crate::dht::forward::forward_presence_lease(dht.clone(), sub.lease).await;
            let fanout = bounded_fanout(
                sub.consents
                    .into_iter()
                    .map(|c| crate::dht::forward::forward_presence_consent(dht.clone(), c))
                    .collect(),
                PRESENCE_FANOUT_CONCURRENCY,
            );
            let _ = tokio::time::timeout(PRESENCE_FANOUT_BUDGET, fanout).await;
        });
    }

    let authorized = authorized_contacts(relay, &contacts, &me);
    let snapshot: Vec<PresenceP> = {
        let online: HashSet<[u8; 32]> = {
            let clients = relay.clients.read();
            let active = relay.active_clients.read();
            authorized
                .iter()
                .copied()
                .filter(|c| {
                    clients.contains_key(c)
                        && active.get(c).is_some_and(|at| {
                            now.saturating_sub(*at) < common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS
                        })
                })
                .collect()
        };
        contacts
            .iter()
            .map(|c| PresenceP {
                who: Bytes(*c),
                state: if !authorized.contains(c) {
                    PresenceState::Offline { last_seen: 0 }
                } else if online.contains(c) {
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
        Some(at) if now.saturating_sub(*at) < common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS => {
            PresenceState::Online
        },
        _ => PresenceState::Offline { last_seen: relay.store.get_last_seen(&me).unwrap_or(0) },
    };
    announce(relay, &contacts, &me, state, now_ms(), &ctx.cancel).await;
    Ok(true)
}

pub(super) async fn handle_set_presence(mode: PresenceMode, ctx: ClientCtxHandle) -> Result<bool> {
    let _update = ctx.relay.presence_update(&ctx.ipk.to_bytes()).await;
    if !is_current(&ctx) || ctx.limits.set_presence.check().is_err() {
        return Ok(false);
    }
    let me = ctx.ipk.to_bytes();
    let relay = &ctx.relay;
    let now = now_ms();
    let state = match mode {
        PresenceMode::Active => {
            relay.store.put_last_seen(&me, now)?;
            relay.store.persist_barrier().wait().await?;
            {
                let clients = relay.clients.read();
                if !clients.get(&me).is_some_and(|conn| conn.stable_id() == ctx.conn.stable_id()) {
                    return Ok(false);
                }
                relay.active_clients.write().insert(me, now);
            }
            PresenceState::Online
        },
        // Only a device that was foreground-Active counts as seen now. A background wake asserts
        // Idle without going Active, so it keeps its prior last-seen.
        PresenceMode::Idle => {
            let was_active = relay.active_clients.write().remove(&me).is_some();
            let last_seen = if was_active {
                relay.store.put_last_seen(&me, now)?;
                relay.store.persist_barrier().wait().await?;
                relay.store.get_last_seen(&me).unwrap_or(now)
            } else {
                relay.store.get_last_seen(&me).unwrap_or(0)
            };
            PresenceState::Offline { last_seen }
        },
    };
    if !is_current(&ctx) {
        return Ok(false);
    }
    let contacts = relay.presence_subs.read().get(&me).cloned().unwrap_or_default();
    announce(relay, &contacts, &me, state, now_ms(), &ctx.cancel).await;
    Ok(true)
}

pub(crate) async fn on_disconnect(relay: &RelayRef, me: &[u8; 32], cancel: &CancellationToken) {
    let _update = relay.presence_update(me).await;
    if relay.clients.read().contains_key(me) {
        return;
    }
    let now = now_ms();
    relay.active_clients.write().remove(me);
    // Abrupt transport loss gives a last observation, not the time the user left.
    let last_seen = relay.store.get_last_seen(me).unwrap_or(0);

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
    // Only interested viewers with the owner's explicit grant receive updates. The owner's
    // own interest set is unrelated to this authorization.
    let interested: HashSet<_> = relay
        .presence_subs
        .read()
        .iter()
        .filter(|(_, theirs)| theirs.contains(me))
        .map(|(viewer, _)| *viewer)
        .collect();
    let viewers: Vec<_> = interested
        .into_iter()
        .filter(|viewer| relay.store.has_presence_consent(me, viewer))
        .collect();
    let targets: Vec<Connection> = {
        let clients = relay.clients.read();
        viewers.iter().filter_map(|c| clients.get(c).cloned()).collect()
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

fn is_current(ctx: &ClientCtxHandle) -> bool {
    ctx.relay
        .clients
        .read()
        .get(&ctx.ipk.to_bytes())
        .is_some_and(|conn| conn.stable_id() == ctx.conn.stable_id())
}

fn authorized_contacts(
    relay: &RelayRef, contacts: &HashSet<[u8; 32]>, me: &[u8; 32],
) -> Vec<[u8; 32]> {
    contacts
        .iter()
        .copied()
        .filter(|contact| relay.store.has_presence_consent(contact, me))
        .collect()
}

fn stored_state(relay: &RelayRef, viewer: &[u8; 32], contact: &[u8; 32]) -> PresenceState {
    if relay.dht.is_none()
        || relay
            .store
            .get_presence_lease(contact)
            .is_some_and(|lease| lease.relay_id == relay.node_id)
    {
        return PresenceState::Offline {
            last_seen: relay.store.get_last_seen(contact).unwrap_or(0),
        };
    }
    relay.store.get_presence_state(viewer, contact).unwrap_or(PresenceState::Offline {
        last_seen: relay.store.get_last_seen(contact).unwrap_or(0),
    })
}

/// A transport kept alive by a background process must not keep an old foreground claim alive.
pub(crate) async fn expire_active(relay: &RelayRef, now: u64, cancel: &CancellationToken) {
    let expired: Vec<_> = relay
        .active_clients
        .read()
        .iter()
        .filter(|(_, at)| now.saturating_sub(**at) >= common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS)
        .map(|(who, _)| *who)
        .collect();
    for who in expired {
        let _update = relay.presence_update(&who).await;
        {
            let mut active = relay.active_clients.write();
            if !active.get(&who).is_some_and(|at| {
                now.saturating_sub(*at) >= common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS
            }) {
                continue;
            }
            active.remove(&who);
        }
        let contacts = relay.presence_subs.read().get(&who).cloned().unwrap_or_default();
        let state =
            PresenceState::Offline { last_seen: relay.store.get_last_seen(&who).unwrap_or(0) };
        announce(relay, &contacts, &who, state, now, cancel).await;
    }
}

pub(crate) async fn maintain(relay: RelayRef, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => expire_active(&relay, now_ms(), &cancel).await,
        }
    }
}

async fn push(conn: &Connection, entries: Vec<PresenceP>) {
    let _ = tokio::time::timeout(STREAM_OPEN_TIMEOUT, async {
        let (mut tx, _rx) = conn.open_bi().await.ok()?;
        SRelayPacket::Presence(entries).send(&mut tx).await.ok()?;
        tx.finish().ok()
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Client, Node, key};
    use common::proto::client_rel::CRelayPacket;
    use common::proto::dht_p2p::{
        PresenceConsent, PresenceLease, presence_consent_signing_input,
        presence_lease_signing_input,
    };
    use common::proto::pack::Unpacker;
    use ed25519_dalek::{Signer, SigningKey};

    fn subscription(
        node: &Node, owner: &SigningKey, contacts: Vec<[u8; 32]>, grants: Vec<([u8; 32], bool)>,
        version: u64,
    ) -> SubscribePresenceP {
        let me = owner.verifying_key().to_bytes();
        let now = now_ms();
        let relay_id = node.relay.node_id;
        let expires = now + common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS;
        SubscribePresenceP {
            contacts: contacts.into_iter().map(Into::into).collect(),
            consents: grants
                .into_iter()
                .map(|(recipient, granted)| PresenceConsent {
                    owner: me.into(),
                    recipient: recipient.into(),
                    version,
                    issued_at_ms: now,
                    granted,
                    user_sig: owner
                        .sign(&presence_consent_signing_input(
                            &me, &recipient, version, now, granted,
                        ))
                        .to_bytes()
                        .into(),
                })
                .collect(),
            lease: PresenceLease {
                user: me.into(),
                relay_id,
                version,
                issued_at_ms: now,
                expires_at_ms: expires,
                user_sig: owner
                    .sign(&presence_lease_signing_input(&me, &relay_id, version, now, expires))
                    .to_bytes()
                    .into(),
            },
        }
    }

    async fn observed(client: &Client, who: [u8; 32]) -> PresenceState {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (_tx, mut rx) = client.connection.accept_bi().await.unwrap();
            let SRelayPacket::Presence(mut list) = SRelayPacket::unpack(&mut rx).await.unwrap()
            else {
                panic!("expected presence")
            };
            assert_eq!(list.len(), 1);
            let entry = list.remove(0);
            assert_eq!(entry.who.0, who);
            entry.state
        })
        .await
        .unwrap()
    }

    async fn accepted(client: &Client, packet: CRelayPacket) {
        assert_eq!(
            client.request(vec![packet]).await,
            vec![SRelayPacket::PresenceAck { accepted: true }]
        );
    }

    #[tokio::test]
    async fn local_presence_is_directional_durable_and_revocable() {
        let node = Node::start(70, false).await;
        let owner = Client::connect(&node).await;
        let viewer = Client::connect(&node).await;
        let (a, b) = (key(71), key(72));
        let (a_id, b_id) = (a.verifying_key().to_bytes(), b.verifying_key().to_bytes());
        assert!(owner.authenticate(&a, &a, &owner.connection).await);
        assert!(viewer.authenticate(&b, &b, &viewer.connection).await);
        // The owner shares but does not subscribe; the viewer reads but grants nothing back.
        accepted(
            &owner,
            CRelayPacket::SubscribePresenceDurable(subscription(
                &node,
                &a,
                vec![],
                vec![(b_id, true)],
                1,
            )),
        )
        .await;
        accepted(
            &viewer,
            CRelayPacket::SubscribePresenceDurable(subscription(&node, &b, vec![a_id], vec![], 1)),
        )
        .await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Offline { last_seen: 0 });
        accepted(&owner, CRelayPacket::SetPresenceDurable(PresenceMode::Active)).await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Online);
        let last_seen = node.relay.store.get_last_seen(&a_id).unwrap();
        assert!(last_seen > 0);
        expire_active(
            &node.relay,
            last_seen + common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Offline { last_seen });
        accepted(&owner, CRelayPacket::SetPresenceDurable(PresenceMode::Active)).await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Online);
        let before_reconnect = node.relay.store.get_last_seen(&a_id).unwrap();
        let replacement = Client::connect(&node).await;
        assert!(replacement.authenticate(&a, &a, &replacement.connection).await);
        let owner = replacement;
        accepted(&owner, CRelayPacket::SetPresenceDurable(PresenceMode::Idle)).await;
        assert_eq!(node.relay.store.get_last_seen(&a_id), Some(before_reconnect));
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Offline { last_seen: before_reconnect });
        accepted(
            &owner,
            CRelayPacket::SubscribePresenceDurable(subscription(
                &node,
                &a,
                vec![],
                vec![(b_id, false)],
                2,
            )),
        )
        .await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Offline { last_seen: 0 });
        accepted(
            &viewer,
            CRelayPacket::SubscribePresenceDurable(subscription(&node, &b, vec![a_id], vec![], 2)),
        )
        .await;
        assert_eq!(observed(&viewer, a_id).await, PresenceState::Offline { last_seen: 0 });
        assert_eq!(
            owner
                .request(vec![CRelayPacket::SubscribePresenceDurable(subscription(
                    &node,
                    &a,
                    vec![],
                    vec![(b_id, true)],
                    1
                ))])
                .await,
            vec![SRelayPacket::PresenceAck { accepted: false }]
        );
        assert!(!node.relay.store.has_presence_consent(&a_id, &b_id));
        let c_id = key(73).verifying_key().to_bytes();
        let mut partial = subscription(&node, &a, vec![], vec![(c_id, true), (b_id, true)], 3);
        let stale = &mut partial.consents[1];
        stale.version = 1;
        stale.user_sig = a.sign(&presence_consent_signing_input(&a_id, &b_id, 1, stale.issued_at_ms, true)).to_bytes().into();
        assert_eq!(owner.request(vec![CRelayPacket::SubscribePresenceDurable(partial)]).await,
            vec![SRelayPacket::PresenceAck { accepted: false }]);
        assert!(!node.relay.store.has_presence_consent(&a_id, &c_id), "a rejected batch must not partially grant access");
    }
}
