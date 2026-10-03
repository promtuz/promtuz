//! Avatar and profile-details reconciliation. Receipts describe stored revisions, independently
//! of relay acceptance. SQL table names come only from `Part`; values are bound parameters.

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::data::conversation::{Conversation, KIND_GROUP};
use crate::data::identity::Identity;
use crate::data::peer_avatar::{self, AvatarUpdate};
use crate::data::peer_profile::{self, ProfileUpdate};
use crate::db::one;
use crate::state::core;

mod routing;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Part {
    Avatar,
    Details,
}
impl Part {
    fn of(payload: &AppPayload) -> Option<Self> {
        match payload {
            AppPayload::Avatar { .. }
            | AppPayload::AvatarAck { .. }
            | AppPayload::AvatarSync { .. } => Some(Self::Avatar),
            AppPayload::ProfileDetails { .. }
            | AppPayload::ProfileDetailsAck { .. }
            | AppPayload::ProfileDetailsSync { .. } => Some(Self::Details),
            _ => None,
        }
    }
    fn stored_table(self) -> &'static str {
        match self {
            Self::Avatar => "peer_avatars",
            Self::Details => "peer_profiles",
        }
    }
    fn ack_table(self) -> &'static str {
        match self {
            Self::Avatar => "avatar_acks",
            Self::Details => "profile_acks",
        }
    }
    fn own(self, identity: &Identity) -> Snapshot {
        match self {
            Self::Avatar => Snapshot::Avatar(identity.avatar_update()),
            Self::Details => Snapshot::Details(identity.details()),
        }
    }
    fn ack(self, revision: u64) -> AppPayload {
        match self {
            Self::Avatar => AppPayload::AvatarAck { revision },
            Self::Details => AppPayload::ProfileDetailsAck { revision },
        }
    }
    fn sync(self, known_revision: Option<u64>, reply: bool) -> AppPayload {
        match self {
            Self::Avatar => AppPayload::AvatarSync { known_revision, reply },
            Self::Details => AppPayload::ProfileDetailsSync { known_revision, reply },
        }
    }
    fn notify_changed(self) {
        match self {
            Self::Avatar => peer_avatar::notify_changed(),
            Self::Details => peer_profile::notify_changed(),
        }
    }
}
#[derive(Clone)]
enum Snapshot {
    Avatar(AvatarUpdate),
    Details(ProfileUpdate),
}
impl Snapshot {
    fn part(&self) -> Part {
        match self {
            Self::Avatar(_) => Part::Avatar,
            Self::Details(_) => Part::Details,
        }
    }
    fn revision(&self) -> u64 {
        match self {
            Self::Avatar(u) => u.revision,
            Self::Details(u) => u.revision,
        }
    }
    fn into_payload(self) -> AppPayload {
        match self {
            Self::Avatar(u) => u.into_payload(),
            Self::Details(u) => u.into_payload(),
        }
    }
}

const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
// Suppress reconnect storms and overlapping relay lifetimes. A reconnect still
// probes a peer even when its earlier ACK was current: it may have lost state.
const PROBE_COOLDOWN: Duration = Duration::from_secs(60);
pub(crate) type PeerKey = ([u8; 32], [u8; 32], Part);

fn known_revision(part: Part, conn: &Connection, peer: &[u8; 32]) -> Result<Option<u64>> {
    let sql = format!("SELECT revision FROM {} WHERE ipk = ?1", part.stored_table());
    Ok(one(conn, &sql, [peer.as_slice()], |r| r.get(0))?)
}

fn acknowledge(
    part: Part, conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], revision: u64, current: u64,
) -> Result<()> {
    // A peer may acknowledge an older snapshot, but cannot pre-ack future edits.
    if revision > current {
        return Ok(());
    }
    let revision = i64::try_from(revision)?;
    let table = part.ack_table();
    conn.execute(
        &format!(
            "INSERT INTO {table} (owner_ipk, peer_ipk, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT(owner_ipk, peer_ipk) DO UPDATE SET revision = excluded.revision
         WHERE excluded.revision > COALESCE({table}.revision, -1)"
        ),
        (owner.as_slice(), peer.as_slice(), revision),
    )?;
    Ok(())
}

/// Apply a control and produce responses in one transaction. Nothing is sent
/// before commit; failed storage must never generate a successful ACK.
fn receive_tx(
    conn: &Connection, own: Option<&([u8; 32], Snapshot)>, peer: &[u8; 32], payload: AppPayload,
) -> Result<(bool, Vec<AppPayload>)> {
    let Some(part) = Part::of(&payload) else { bail!("not a profile control") };
    if let Some((_, update)) = own {
        anyhow::ensure!(update.part() == part, "profile field mismatch");
    }
    let tx = conn.unchecked_transaction()?;
    let mut changed = false;
    let mut responses = Vec::new();
    match payload {
        AppPayload::Avatar { revision, avif } => {
            changed = peer_avatar::apply_tx(&tx, peer, &AvatarUpdate { revision, avif })?;
            // A delayed upload after a removal acknowledges the retained newer
            // tombstone, not the stale incoming bytes. Duplicates also re-ACK.
            if let Some(revision) = known_revision(part, &tx, peer)? {
                responses.push(part.ack(revision));
            }
        },
        AppPayload::ProfileDetails { revision, name, bio, card } => {
            changed =
                peer_profile::apply_tx(&tx, peer, &ProfileUpdate { revision, name, bio, card })?;
            if let Some(revision) = known_revision(part, &tx, peer)? {
                responses.push(part.ack(revision));
            }
        },
        AppPayload::AvatarAck { revision } | AppPayload::ProfileDetailsAck { revision } => {
            if let Some((owner, update)) = own {
                acknowledge(part, &tx, owner, peer, revision, update.revision())?;
            }
        },
        AppPayload::AvatarSync { known_revision: received, reply }
        | AppPayload::ProfileDetailsSync { known_revision: received, reply } => {
            let Some((owner, update)) = own else { bail!("identity unavailable") };
            let table = part.ack_table();
            // The exchange proves support for this field, independently of the other.
            tx.execute(
                &format!("INSERT OR IGNORE INTO {table} (owner_ipk, peer_ipk) VALUES (?1, ?2)"),
                (owner.as_slice(), peer.as_slice()),
            )?;
            if received != Some(update.revision()) {
                // A restored peer may have no copy OR an older copy. Either
                // claim invalidates our previous evidence of their latest copy.
                tx.execute(
                    &format!(
                        "UPDATE {table} SET revision = NULL WHERE owner_ipk = ?1 AND peer_ipk = ?2"
                    ),
                    (owner.as_slice(), peer.as_slice()),
                )?;
            }
            if let Some(revision) = received {
                acknowledge(part, &tx, owner, peer, revision, update.revision())?;
            }
            if !reply {
                responses.push(part.sync(known_revision(part, &tx, peer)?, true));
            }
            if received != Some(update.revision()) {
                responses.push(update.clone().into_payload());
            }
        },
        _ => bail!("not a profile control"),
    }
    tx.commit()?;
    Ok((changed, responses))
}

/// `peer` is the authenticated MLS author, never a key supplied in the payload.
pub(crate) fn receive(conversation: [u8; 16], peer: [u8; 32], payload: AppPayload) {
    let Some(part) = Part::of(&payload) else { return };
    // Never nest the identity lock inside the messages lock.
    let own = Identity::get().map(|i| (i.ipk(), part.own(&i)));
    let result = receive_tx(&core().db.messages().lock(), own.as_ref(), &peer, payload);
    match result {
        Ok((changed, responses)) => {
            if changed {
                part.notify_changed();
            }
            // A requester may tell us about themselves; we answer once accepted.
            if responses.is_empty() || crate::requests::is_request_chat(&conversation) {
                return;
            }
            core().spawn(async move {
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
    part: Part, conn: &Connection, owner: &[u8; 32], peer: &[u8; 32], current: u64,
) -> Result<bool> {
    // Unknown/old clients are probed on reconnect, not repeatedly disturbed by
    // new control variants. Once support is established, retry missing ACKs.
    let sql =
        format!("SELECT revision FROM {} WHERE owner_ipk = ?1 AND peer_ipk = ?2", part.ack_table());
    let ack: Option<Option<u64>> =
        one(conn, &sql, (owner.as_slice(), peer.as_slice()), |r| r.get(0))?;
    Ok(ack.is_some_and(|revision| revision != Some(current)))
}

fn claim_probe(probes: &mut HashMap<PeerKey, Instant>, key: PeerKey, now: Instant) -> bool {
    if probes.get(&key).is_some_and(|last| now.duration_since(*last) < PROBE_COOLDOWN) {
        return false;
    }
    probes.insert(key, now);
    true
}

async fn reconcile_part(
    part: Part, owner: [u8; 32], current: u64, peers: &HashMap<[u8; 32], Vec<[u8; 16]>>, all: bool,
) -> Result<()> {
    let now = Instant::now();
    for (peer, routes) in peers {
        let known = {
            let conn = core().db.messages().lock();
            if !all && !should_retry(part, &conn, &owner, peer, current)? {
                continue;
            }
            known_revision(part, &conn, peer)?
        };
        if !claim_probe(&mut core().profile_probes.lock(), (owner, *peer, part), now) {
            continue;
        }
        if let Err(e) = routing::probe(*peer, routes, part.sync(known, false)).await {
            log::debug!("PROFILE: {part:?} probe for {} failed: {e}", hex::encode(&peer[..4]));
        }
    }
    Ok(())
}

async fn reconcile_group_pictures(owner: &[u8; 32], all: bool) {
    if !all {
        return;
    }
    for chat in Conversation::list()
        .into_iter()
        .filter(|c| c.kind == KIND_GROUP && c.mls_group_id.is_some())
    {
        if Conversation::may_edit(&chat.id, owner) {
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
async fn reconcile(all: bool) {
    let Some(identity) = Identity::get() else { return };
    let owner = identity.ipk();
    let avatar = identity.avatar_update().revision;
    let details = identity.details().revision;
    let peers = routing::routes(&owner);
    core()
        .profile_probes
        .lock()
        .retain(|(who, peer, _), _| *who == owner && peers.contains_key(peer));
    let (avatar, details, ()) = tokio::join!(
        reconcile_part(Part::Avatar, owner, avatar, &peers, all),
        reconcile_part(Part::Details, owner, details, &peers, all),
        reconcile_group_pictures(&owner, all),
    );
    for (part, result) in [(Part::Avatar, avatar), (Part::Details, details)] {
        if let Err(e) = result {
            log::warn!("PROFILE: {part:?} reconciliation failed: {e}");
        }
    }
}

pub(crate) async fn run(cancel: CancellationToken) {
    let mut all = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = reconcile(all) => {},
        }
        all = false;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {},
        }
    }
}
