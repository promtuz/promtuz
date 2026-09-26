//! Repair profile state independently of message delivery. Relay acceptance
//! cannot tell us whether the receiving app understood and stored profile details.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use common::proto::mls_wire::AppPayload;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::peer_profile::{self, ProfileUpdate};
use crate::db::messages::MESSAGES_DB;

const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
// Suppress reconnect storms and overlapping relay lifetimes. A reconnect still
// probes a peer even when its earlier ACK was current: it may have lost state.
const PROBE_COOLDOWN: Duration = Duration::from_secs(60);
type PeerKey = ([u8; 32], [u8; 32]);
static PROBES: Lazy<Mutex<HashMap<PeerKey, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn known_revision(conn: &Connection, peer: &[u8; 32]) -> Result<Option<u64>> {
    Ok(conn
        .query_row("SELECT revision FROM peer_profiles WHERE ipk = ?1", [peer.as_slice()], |r| {
            r.get(0)
        })
        .optional()?)
}

fn acknowledge(
    conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], revision: u64, current: u64,
) -> Result<()> {
    // A peer may acknowledge an older snapshot, but cannot pre-ack future edits.
    if revision > current {
        return Ok(());
    }
    let revision = i64::try_from(revision)?;
    conn.execute(
        "INSERT INTO profile_acks (owner_ipk, peer_ipk, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT(owner_ipk, peer_ipk) DO UPDATE SET revision = excluded.revision
         WHERE excluded.revision > COALESCE(profile_acks.revision, -1)",
        (owner.as_slice(), peer.as_slice(), revision),
    )?;
    Ok(())
}

/// Apply a control and produce responses in one transaction. Nothing is sent
/// before commit; failed storage must never generate a successful ACK.
fn receive_tx(
    conn: &Connection, own: Option<&([u8; 32], ProfileUpdate)>, peer: &[u8; 32],
    payload: AppPayload,
) -> Result<(bool, Vec<AppPayload>)> {
    let tx = conn.unchecked_transaction()?;
    let mut changed = false;
    let mut responses = Vec::new();
    match payload {
        AppPayload::ProfileDetails { revision, name, bio, card } => {
            changed =
                peer_profile::apply_tx(&tx, peer, &ProfileUpdate { revision, name, bio, card })?;
            // A delayed update acknowledges the retained newer revision,
            // not the stale incoming fields. Duplicates also re-ACK.
            if let Some(revision) = known_revision(&tx, peer)? {
                responses.push(AppPayload::ProfileDetailsAck { revision });
            }
        },
        AppPayload::ProfileDetailsAck { revision } => {
            if let Some((owner, update)) = own {
                acknowledge(&tx, owner, peer, revision, update.revision)?;
            }
        },
        AppPayload::ProfileDetailsSync { known_revision: received, reply } => {
            let Some((owner, update)) = own else { bail!("identity unavailable") };
            // The exchange proves support even if they have never seen profile details.
            tx.execute(
                "INSERT OR IGNORE INTO profile_acks (owner_ipk, peer_ipk) VALUES (?1, ?2)",
                (owner.as_slice(), peer.as_slice()),
            )?;
            if received != Some(update.revision) {
                // A restored peer may have no copy OR an older copy. Either
                // claim invalidates our previous evidence of their latest copy.
                tx.execute(
                    "UPDATE profile_acks SET revision = NULL WHERE owner_ipk = ?1 AND peer_ipk = ?2",
                    (owner.as_slice(), peer.as_slice()),
                )?;
            }
            if let Some(revision) = received {
                acknowledge(&tx, owner, peer, revision, update.revision)?;
            }
            if !reply {
                responses.push(AppPayload::ProfileDetailsSync {
                    known_revision: known_revision(&tx, peer)?,
                    reply: true,
                });
            }
            if received != Some(update.revision) {
                responses.push(update.clone().into_payload());
            }
        },
        _ => bail!("not a profile control"),
    }
    tx.commit()?;
    Ok((changed, responses))
}

/// Shared by live delivery and messages released from MLS epoch catch-up.
/// `peer` is the authenticated MLS author, never a key supplied in the payload.
pub(crate) fn receive(conversation: [u8; 16], peer: [u8; 32], payload: AppPayload) {
    // Never nest the identity lock inside the messages lock.
    let own = Identity::get().map(|i| (i.ipk(), i.details()));
    let result = receive_tx(&MESSAGES_DB.lock(), own.as_ref(), &peer, payload);
    match result {
        Ok((changed, responses)) => {
            if changed {
                peer_profile::notify_changed();
            }
            if responses.is_empty() {
                return;
            }
            crate::RUNTIME.spawn(async move {
                for response in responses {
                    if let Err(e) =
                        crate::messaging::send_control_to(conversation, response, peer).await
                    {
                        log::debug!("PROFILE: reconciliation response failed: {e}");
                    }
                }
            });
        },
        Err(e) => log::warn!("PROFILE: could not persist profile control: {e}"),
    }
}

fn should_retry(
    conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], current: u64,
) -> Result<bool> {
    // Unknown/old clients are probed on reconnect, not repeatedly disturbed by
    // new control variants. Once support is established, retry missing ACKs.
    let ack: Option<Option<u64>> = conn
        .query_row(
            "SELECT revision FROM profile_acks WHERE owner_ipk = ?1 AND peer_ipk = ?2",
            (owner.as_slice(), peer.as_slice()),
            |r| r.get(0),
        )
        .optional()?;
    Ok(ack.is_some_and(|revision| revision != Some(current)))
}

fn claim_probe(probes: &mut HashMap<PeerKey, Instant>, key: PeerKey, now: Instant) -> bool {
    if probes.get(&key).is_some_and(|last| now.duration_since(*last) < PROBE_COOLDOWN) {
        return false;
    }
    probes.insert(key, now);
    true
}

async fn reconcile(all: bool) -> Result<()> {
    let Some(identity) = Identity::get() else { return Ok(()) };
    let owner = identity.ipk();
    let current = identity.details().revision;
    let mut peers = HashMap::new();
    // One shared active chat per peer is enough, even if we share many groups.
    for chat in Conversation::list().into_iter().filter(|c| c.mls_group_id.is_some()) {
        let members = Conversation::members(&chat.id);
        if !members.iter().any(|m| m.active && m.member_ipk == owner) {
            continue;
        }
        for member in members.into_iter().filter(|m| m.active && m.member_ipk != owner) {
            peers.entry(member.member_ipk).or_insert(chat.id);
        }
    }
    if all {
        for chat in Conversation::list()
            .into_iter()
            .filter(|c| c.kind == crate::data::conversation::KIND_GROUP && c.mls_group_id.is_some())
        {
            if Conversation::is_admin(&chat.id, &owner) {
                if let Some((revision, avif)) = crate::data::group_picture::snapshot(&chat.id) {
                    let _ = crate::messaging::send_control(
                        chat.id,
                        AppPayload::GroupPicture { revision, avif },
                    )
                    .await;
                }
            }
        }
    }
    let now = Instant::now();
    PROBES.lock().retain(|(who, peer), _| *who == owner && peers.contains_key(peer));
    for (peer, conversation) in peers {
        let known = {
            let conn = MESSAGES_DB.lock();
            if !all && !should_retry(&conn, &owner, &peer, current)? {
                continue;
            }
            known_revision(&conn, &peer)?
        };
        if !claim_probe(&mut PROBES.lock(), (owner, peer), now) {
            continue;
        }
        if let Err(e) = crate::messaging::send_control_to(
            conversation,
            AppPayload::ProfileDetailsSync { known_revision: known, reply: false },
            peer,
        )
        .await
        {
            log::debug!("PROFILE: reconciliation probe failed: {e}");
        }
    }
    Ok(())
}

pub(crate) async fn run(cancel: CancellationToken) {
    let mut all = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = reconcile(all) => {
                if let Err(e) = result { log::warn!("PROFILE: reconciliation failed: {e}"); }
            },
        }
        all = false;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_metadata_cannot_restore_a_removed_bio_and_failed_commit_never_acks() {
        let db = crate::db::messages::open_in_memory();
        let who = [42; 32];
        let latest = AppPayload::ProfileDetails {
            revision: 20,
            name: "New".into(),
            bio: "".into(),
            card: vec![],
        };
        let (_, ack) = receive_tx(&db, None, &who, latest).unwrap();
        assert!(matches!(ack.as_slice(), [AppPayload::ProfileDetailsAck { revision: 20 }]));
        let old = AppPayload::ProfileDetails {
            revision: 10,
            name: "Old".into(),
            bio: "Removed".into(),
            card: vec![],
        };
        let (changed, ack) = receive_tx(&db, None, &who, old).unwrap();
        assert!(!changed);
        assert!(matches!(ack.as_slice(), [AppPayload::ProfileDetailsAck { revision: 20 }]));
        let bio: String = db.query_row("SELECT bio FROM peer_profiles", [], |r| r.get(0)).unwrap();
        assert!(bio.is_empty());
        db.execute_batch("CREATE TRIGGER fail_profile AFTER UPDATE ON peer_profiles BEGIN SELECT RAISE(ABORT,'disk full'); END;").unwrap();
        assert!(
            receive_tx(
                &db,
                None,
                &who,
                AppPayload::ProfileDetails {
                    revision: 30,
                    name: "Failed".into(),
                    bio: "x".into(),
                    card: vec![]
                }
            )
            .is_err()
        );
        let revision: u64 =
            db.query_row("SELECT revision FROM peer_profiles", [], |r| r.get(0)).unwrap();
        assert_eq!(revision, 20);
    }
}
