//! Presence and typing. Neither goes through MLS.

use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::crypto::verify_ed25519;
use common::proto::client_rel::ActivityP;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::proto::client_rel::activity_sig_message;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use ed25519_dalek::VerifyingKey;
use parking_lot::Mutex as PlMutex;

use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::events::Emittable;
use crate::quic::server::Session;
use crate::state::core;

/// Signed but not encrypted, and never queued by the relay: dropped if either side is offline.
pub async fn set_activity(
    session: Option<&Session>, conversation: [u8; 16], activity: u16,
) -> Result<()> {
    // Even "present" would tell a requester we opened their chat.
    if crate::requests::is_request_chat(&conversation) {
        return Ok(());
    }
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    // Before pairing establishes a group there is no shared chat to address.
    let Some(group_id) = Conversation::group_of(&conversation) else { return Ok(()) };
    let ts = now_ms();

    let Some(session) = session else { return Ok(()) };

    // The signature binds `to`, so each member gets their own copy. Nothing is outboxed: a late
    // typing signal is worthless.
    for peer in Conversation::recipients(&conversation) {
        let sig = crate::data::identity::IdentitySigner::sign(&activity_sig_message(
            &peer, &our_ipk, &group_id, activity, ts,
        ))
        .map_err(|e| anyhow!("sign ephemeral: {e}"))?
        .to_bytes();
        let eph = ActivityP {
            to: Bytes(peer),
            from: Bytes(our_ipk),
            group_id: Bytes(group_id),
            activity,
            timestamp: ts,
            sig: Bytes(sig),
        };
        let bytes =
            CRelayPacket::Activity(eph).pack().map_err(|e| anyhow!("pack ephemeral: {e}"))?;
        if let Ok((mut tx, _rx)) = session.conn.open_bi().await {
            let _ = tx.write_all(&bytes).await;
            let _ = tx.finish();
        }
    }
    Ok(())
}

/// Replaces the prior interest set; the relay pushes a snapshot, then deltas.
pub async fn subscribe_presence(session: Option<&Session>, contacts: Vec<[u8; 32]>) -> Result<()> {
    let _update = core().presence_update.lock().await;
    subscribe_presence_in(core().db.network(), session, contacts).await
}

/// Desired reads survive offline changes; only grants that may have been sent enter the ledger.
async fn subscribe_presence_in(
    network: &PlMutex<rusqlite::Connection>, session: Option<&Session>, mut contacts: Vec<[u8; 32]>,
) -> Result<()> {
    contacts.sort_unstable();
    contacts.dedup();
    anyhow::ensure!(
        contacts.len() <= common::proto::client_rel::MAX_PRESENCE_CONTACTS,
        "too many presence contacts"
    );
    {
        let mut conn = network.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM presence_interest", [])?;
        for peer in &contacts {
            tx.execute("INSERT INTO presence_interest(peer) VALUES (?1)", [peer.as_slice()])?;
        }
        tx.commit()?;
    }
    let session = session.ok_or_else(|| anyhow!("presence: no relay connection"))?;
    send_presence_subscription(network, session, contacts).await
}

/// Read the desired set after taking the same lock as edits, so a waiting renewal cannot
/// overwrite a newer user choice with a snapshot taken before the edit.
async fn renew_presence_lease(session: &Session) -> Result<()> {
    let _update = core().presence_update.lock().await;
    let contacts = {
        let conn = core().db.network().lock();
        let mut stmt = conn.prepare("SELECT peer FROM presence_interest")?;
        stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .map(|peer| Ok(peer?.try_into().map_err(|_| anyhow!("invalid presence peer"))?))
            .collect::<Result<Vec<[u8; 32]>>>()?
    };
    send_presence_subscription(core().db.network(), session, contacts).await
}

async fn send_presence_subscription(
    network: &PlMutex<rusqlite::Connection>, session: &Session, contacts: Vec<[u8; 32]>,
) -> Result<()> {
    use common::proto::client_rel::MAX_PRESENCE_CONSENTS;
    require_durable_presence(session).await?;
    let relay_id = match session.home_node_id {
        Some(id) => common::quic::id::NodeId::from_bytes(id),
        None => session.relay.id.parse().map_err(|_| anyhow!("invalid relay storage identity"))?,
    };
    let me = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    let signer = crate::data::identity::secret_key_signing(&me)?;
    let desired: std::collections::BTreeSet<_> = contacts
        .iter()
        .copied()
        .filter(|peer| Contact::is_paired(peer) && !crate::requests::is_blocked(peer))
        .collect();
    let interests = contacts;
    anyhow::ensure!(
        interests.len() <= common::proto::client_rel::MAX_PRESENCE_CONTACTS,
        "too many presence contacts"
    );
    let previous = persisted_presence_contacts_tx(&network.lock())?;
    let revoked: Vec<_> = previous.into_iter().filter(|p| !desired.contains(p)).collect();
    let batch_size = MAX_PRESENCE_CONSENTS - desired.len();
    // Include one request even when there is nothing to revoke: it renews the lease and reads.
    for removed in
        revoked.chunks(batch_size).chain(if revoked.is_empty() { Some(&[][..]) } else { None })
    {
        let now = now_ms();
        let version = next_presence_lease_version_tx(&network.lock(), now)?;
        let sub = subscription(&signer, relay_id, &interests, &desired, removed, version, now);
        // A lost ACK may have installed these grants. Persist before sending, but never add
        // offline edits that have not reached this point.
        {
            let mut conn = network.lock();
            let tx = conn.transaction()?;
            for peer in &desired {
                tx.execute(
                    "INSERT OR IGNORE INTO presence_contacts(peer) VALUES (?1)",
                    [peer.as_slice()],
                )?;
            }
            tx.commit()?;
        }
        presence_request(session, CRelayPacket::SubscribePresenceDurable(sub)).await?;
        {
            let mut conn = network.lock();
            let tx = conn.transaction()?;
            for peer in removed {
                tx.execute("DELETE FROM presence_contacts WHERE peer=?1", [peer.as_slice()])?;
            }
            tx.commit()?;
        }
    }
    Ok(())
}

fn subscription(
    signer: &ed25519_dalek::SigningKey, relay_id: common::quic::id::NodeId, contacts: &[[u8; 32]],
    desired: &std::collections::BTreeSet<[u8; 32]>, removed: &[[u8; 32]], version: u64, now: u64,
) -> SubscribePresenceP {
    use common::proto::dht_p2p::{
        PresenceConsent, PresenceLease, presence_consent_signing_input,
        presence_lease_signing_input,
    };
    use ed25519_dalek::Signer;
    let me = signer.verifying_key().to_bytes();
    let consents = desired
        .iter()
        .map(|recipient| (*recipient, true))
        .chain(removed.iter().map(|recipient| (*recipient, false)))
        .map(|(recipient, granted)| PresenceConsent {
            owner: me.into(),
            recipient: recipient.into(),
            version,
            issued_at_ms: now,
            granted,
            user_sig: signer
                .sign(&presence_consent_signing_input(&me, &recipient, version, now, granted))
                .to_bytes()
                .into(),
        })
        .collect();
    let expires_at_ms = now + common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS;
    let lease = PresenceLease {
        user: me.into(),
        relay_id,
        version,
        issued_at_ms: now,
        expires_at_ms,
        user_sig: signer
            .sign(&presence_lease_signing_input(&me, &relay_id, version, now, expires_at_ms))
            .to_bytes()
            .into(),
    };
    SubscribePresenceP { contacts: contacts.iter().copied().map(Bytes).collect(), consents, lease }
}

pub(crate) fn persisted_presence_contacts_tx(conn: &rusqlite::Connection) -> Result<Vec<[u8; 32]>> {
    let mut stmt = conn.prepare("SELECT peer FROM presence_contacts")?;
    Ok(stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .filter_map(|peer| peer.ok().and_then(|peer| peer.try_into().ok()))
        .collect())
}

#[cfg(test)]
fn replace_presence_contacts_tx(conn: &rusqlite::Connection, contacts: &[[u8; 32]]) -> Result<()> {
    conn.execute("DELETE FROM presence_contacts", [])?;
    for peer in contacts {
        conn.execute(
            "INSERT OR IGNORE INTO presence_contacts(peer) VALUES (?1)",
            [peer.as_slice()],
        )?;
    }
    Ok(())
}

pub(crate) fn next_presence_lease_version_tx(conn: &rusqlite::Connection, now: u64) -> Result<u64> {
    let old = conn
        .query_row("SELECT lease_version FROM presence_state WHERE singleton = 1", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);
    let version = old.max(now).saturating_add(1);
    conn.execute(
        "INSERT INTO presence_state(singleton, lease_version) VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET lease_version = excluded.lease_version",
        [version],
    )?;
    Ok(version)
}

/// Whether the user is in the app. The renewal loop asks before spending a
/// lease renewal and a K-home publish on a connection nobody is looking at.
pub fn presence_is_active() -> bool {
    !core().presence_idle.load(Ordering::Relaxed)
}

/// `idle` is sent as the last packet before the app freezes, `false` on return.
pub async fn set_presence(session: Option<&Session>, idle: bool) -> Result<()> {
    core().presence_idle.store(idle, Ordering::Relaxed);
    send_presence(session, idle).await
}

/// Lease first, then the state: the relay signs the state against its current lease, and a home
/// rejects a record whose lease has expired.
pub async fn renew_presence(session: &Session) -> Result<()> {
    renew_presence_lease(session).await?;
    send_presence(Some(session), false).await
}

async fn send_presence(session: Option<&Session>, idle: bool) -> Result<()> {
    let mode = if idle {
        common::proto::client_rel::PresenceMode::Idle
    } else {
        common::proto::client_rel::PresenceMode::Active
    };
    let Some(session) = session else { return Ok(()) };
    let _update = core().presence_update.lock().await;
    require_durable_presence(session).await?;
    presence_request(session, CRelayPacket::SetPresenceDurable(mode)).await
}

async fn require_durable_presence(session: &Session) -> Result<()> {
    use common::contracts::services;
    anyhow::ensure!(
        session
            .services()
            .await
            .supports(services::DURABLE_PRESENCE, services::DURABLE_PRESENCE_VERSION),
        "relay does not support durable presence"
    );
    Ok(())
}

async fn presence_request(session: &Session, packet: CRelayPacket) -> Result<()> {
    use common::proto::Sender;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (mut tx, mut rx) = session.conn.open_bi().await?;
        packet.send(&mut tx).await?;
        tx.finish()?;
        if !matches!(
            SRelayPacket::unpack(&mut rx).await?,
            SRelayPacket::PresenceAck { accepted: true }
        ) {
            bail!("relay rejected presence update");
        }
        Ok(())
    })
    .await?
}

/// Only while the user is in the app: idle is already the default on a fresh connection, and
/// saying so would cost a K-home publish.
pub async fn reassert_presence(session: &Session) -> Result<()> {
    renew_presence_lease(session).await?;
    if !presence_is_active() {
        return Ok(());
    }
    set_presence(Some(session), false).await
}

/// Never stored; a forged signal or a stranger's is dropped silently.
pub(crate) fn handle_activity(our_ipk: VerifyingKey, eph: common::proto::client_rel::ActivityP) {
    if eph.to.as_slice() != our_ipk.as_bytes().as_slice() {
        return;
    }
    let transcript = common::proto::client_rel::activity_sig_message(
        &eph.to.0,
        &eph.from.0,
        &eph.group_id.0,
        eph.activity,
        eph.timestamp,
    );
    if verify_ed25519(&eph.from.0, &transcript, &eph.sig.0).is_err() {
        return;
    }
    // Same standing as a message: someone in a group with us may show as
    // typing in it, address book or not.
    if !Contact::exists(&eph.from.0) && !Conversation::shares_a_chat_with(&eph.from.0) {
        return;
    }
    // Translate the shared wire identity into our own conversation, and only
    // accept a signal from an active member of that specific chat.
    let Some(conversation) = Conversation::for_activity(&eph.group_id.0, &eph.from.0) else {
        return;
    };
    crate::events::messaging::ActivityEv { conversation, peer: eph.from.0, activity: eph.activity }
        .emit();
}

/// Unsigned because the relay is the presence authority; non-contacts are still dropped.
pub(crate) fn handle_presence(list: Vec<common::proto::client_rel::PresenceP>) {
    use common::proto::client_rel::PresenceState;

    use crate::platform::Presence;
    for e in list {
        if !Contact::exists(&e.who.0) {
            continue;
        }
        // A contact coming online is the moment a held download can move:
        // the sender we could not reach is reachable now.
        if matches!(e.state, PresenceState::Online) {
            crate::transfer::resume_for_peer(e.who.0);
        }
        let presence = match e.state {
            PresenceState::Online => Presence::Online,
            PresenceState::Idle { since } => Presence::Idle { since },
            PresenceState::Offline { last_seen } => Presence::Offline { last_seen },
        };
        crate::events::messaging::PresenceEv { peer: e.who.0, presence }.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Stores;

    #[tokio::test]
    async fn offline_edits_and_lost_acks_keep_revocations_bounded() {
        use crate::test_support::{ScopedCore, data, net};
        use common::contracts::{Support, services};
        use common::proto::Sender;
        let scope = ScopedCore::new();
        data::identity(&scope.core.db.identity().lock(), 60);
        let network = scope.core.db.network();
        let peer = |n: u32| {
            let mut p = [0; 32];
            p[..4].copy_from_slice(&n.to_be_bytes());
            p
        };
        for start in [0, 256, 512] {
            assert!(
                subscribe_presence_in(network, None, (start..start + 256).map(peer).collect())
                    .await
                    .is_err()
            );
        }
        assert!(
            persisted_presence_contacts_tx(&network.lock()).unwrap().is_empty(),
            "unsent edits are not possible grants"
        );
        // Recover a ledger accumulated by the previous implementation.
        replace_presence_contacts_tx(&network.lock(), &(0..768).map(peer).collect::<Vec<_>>())
            .unwrap();
        let accepted = peer(900);
        let pending = peer(901);
        Contact::save(accepted, "Accepted".into()).unwrap();
        Contact::save_pending(pending, "Pending".into()).unwrap();
        let (ours, theirs) = net::connection().await;
        let mut session = net::session(ours);
        session.home_node_id = Some([7; 32]);
        let task = tokio::spawn(async move {
            let mut requests = 0;
            let mut last_version = 0;
            while let Ok((mut tx, mut rx)) = theirs.accept_bi().await {
                let reply = match CRelayPacket::unpack(&mut rx).await.unwrap() {
                    CRelayPacket::ServiceCapabilities => SRelayPacket::ServiceCapabilities {
                        supported: Support::new([(services::DURABLE_PRESENCE, vec![1])])
                            .unwrap()
                            .encode()
                            .into(),
                    },
                    CRelayPacket::SubscribePresenceDurable(sub) => {
                        requests += 1;
                        assert!(
                            sub.consents.len() <= common::proto::client_rel::MAX_PRESENCE_CONSENTS
                        );
                        assert_eq!(sub.contacts, [Bytes(accepted), Bytes(pending)]);
                        assert_eq!(
                            sub.consents
                                .iter()
                                .filter(|c| c.granted)
                                .map(|c| c.recipient.0)
                                .collect::<Vec<_>>(),
                            [accepted]
                        );
                        assert!(sub.lease.verify(now_ms()));
                        assert!(sub.consents.iter().all(|c| c.verify(now_ms())));
                        assert!(sub.lease.version > last_version);
                        last_version = sub.lease.version;
                        if requests == 2 {
                            tx.reset(0u32.into()).unwrap(); // Accepted by the relay, ACK lost.
                            continue;
                        }
                        SRelayPacket::PresenceAck { accepted: true }
                    },
                    packet => panic!("unexpected presence request: {packet:?}"),
                };
                reply.send(&mut tx).await.unwrap();
                tx.finish().unwrap();
            }
            requests
        });
        assert!(
            subscribe_presence_in(network, Some(&session), vec![accepted, pending]).await.is_err()
        );
        assert_eq!(
            persisted_presence_contacts_tx(&network.lock()).unwrap().len(),
            258,
            "only acknowledged revocations leave the ledger"
        );
        subscribe_presence_in(network, Some(&session), vec![accepted, pending]).await.unwrap();
        assert_eq!(persisted_presence_contacts_tx(&network.lock()).unwrap(), [accepted]);
        session.conn.close(0u32.into(), b"test-complete");
        assert_eq!(task.await.unwrap(), 3);
    }

    /// A contact dropped while offline is still revoked by the next subscription that is sent.
    #[tokio::test]
    async fn a_presence_revocation_survives_an_offline_change() {
        let db = Stores::in_memory(String::new());
        let (kept, dropped) = ([1; 32], [2; 32]);
        replace_presence_contacts_tx(&db.network().lock(), &[kept, dropped]).unwrap();
        assert!(subscribe_presence_in(db.network(), None, vec![kept]).await.is_err());
        let mut persisted = persisted_presence_contacts_tx(&db.network().lock()).unwrap();
        persisted.sort_unstable();
        assert_eq!(persisted, [kept, dropped]);
        let desired: Vec<Vec<u8>> = db
            .network()
            .lock()
            .prepare("SELECT peer FROM presence_interest")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(desired, [kept.to_vec()], "a retry must not restore the dropped contact");
    }
}
