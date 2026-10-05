//! Latest encrypted fields on one explicitly pinned relay. No profile messages enter the outbox.
use super::crypto;
use crate::{
    data::{
        contact::{Contact, PAIR_STATUS_PAIRED},
        identity::Identity,
    },
    db::one,
    state::core,
};
use anyhow::{Result, anyhow, bail, ensure};
use common::{
    contracts::services,
    crypto::get_nonce,
    proto::{
        Sender,
        client_rel::{CRelayPacket, SRelayPacket},
        pack::{Packer, Unpacker},
        profile::*,
    },
    utils::now_ms,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, uniffi::Enum)]
pub enum ProfileField {
    Name,
    Bio,
    Avatar,
}
impl From<ProfileField> for Field {
    fn from(f: ProfileField) -> Self {
        match f {
            ProfileField::Name => Self::Name,
            ProfileField::Bio => Self::Bio,
            ProfileField::Avatar => Self::Avatar,
        }
    }
}
#[derive(Serialize, Deserialize, uniffi::Record)]
pub struct ProfileSharing {
    pub accepted_contacts: bool,
    pub excluded: Vec<Vec<u8>>,
}
fn policy_key(field: Field) -> String {
    format!("profile-sharing:{}", field.id())
}
fn policy(field: Field) -> Result<ProfileSharing> {
    let stored: Option<String> = one(
        &core().db.messages().lock(),
        "SELECT value FROM app_prefs WHERE key=?1",
        [policy_key(field)],
        |r| r.get(0),
    )?;
    stored
        .map(|s| -> Result<_> { Ok(ProfileSharing::deser(&hex::decode(s)?)?) })
        .unwrap_or_else(|| Ok(ProfileSharing { accepted_contacts: true, excluded: vec![] }))
}
#[uniffi::export]
pub fn profile_sharing(field: ProfileField) -> Result<ProfileSharing, crate::platform::CoreError> {
    Ok(policy(field.into())?)
}
#[uniffi::export]
pub fn set_profile_sharing(
    field: ProfileField, mut sharing: ProfileSharing,
) -> Result<(), crate::platform::CoreError> {
    if sharing.excluded.len() > MAX_READERS || sharing.excluded.iter().any(|p| p.len() != 32) {
        return Err(anyhow!("invalid profile exclusions").into());
    }
    sharing.excluded.sort();
    sharing.excluded.dedup();
    crate::data::app_prefs::set(
        &policy_key(field.into()),
        &hex::encode(sharing.ser().map_err(anyhow::Error::from)?),
    )?;
    wake();
    Ok(())
}
pub(crate) fn wake() {
    core().profile_publish.store(true, std::sync::atomic::Ordering::Release);
    core().profile_changed.notify_one();
}
pub(crate) fn invalidate(owner: [u8; 32]) {
    let mut pending = core().profile_refresh.lock();
    if pending.len() < MAX_READERS {
        pending.insert(owner);
    }
    core().profile_changed.notify_one();
}

/// Legacy publication is restricted too; a group encryption context is not a private pair.
pub(crate) fn allows(field: Field, peer: &[u8; 32]) -> bool {
    Contact::is_paired(peer)
        && !crate::requests::is_blocked(peer)
        && policy(field)
            .is_ok_and(|p| p.accepted_contacts && !p.excluded.iter().any(|p| p.as_slice() == peer))
}
fn host(owner: &[u8; 32]) -> Result<Option<String>> {
    Ok(one(
        &core().db.messages().lock(),
        "SELECT value FROM app_prefs WHERE key=?1",
        [format!("profile-host:{}", hex::encode(owner))],
        |r| r.get(0),
    )?)
}
pub(crate) async fn uses_service() -> bool {
    if Identity::local_ipk().is_some_and(|owner| host(&owner).map_or(true, |h| h.is_some())) {
        return true;
    }
    match core().session() {
        Some(s) => {
            s.services().await.supports(services::PROFILE_STORE, services::PROFILE_STORE_VERSION)
        },
        None => false,
    }
}
async fn rpc(session: &crate::quic::server::Session, request: Request) -> Result<Response> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (mut tx, mut rx) = session.conn.open_bi().await?;
        CRelayPacket::Profile(request).send(&mut tx).await?;
        tx.finish()?;
        let SRelayPacket::Profile(reply) = SRelayPacket::unpack(&mut rx).await? else {
            bail!("unexpected profile response")
        };
        Ok(reply)
    })
    .await?
}

/// At most eight RPCs in flight; results keep request order. The caller bounds each batch.
async fn batch(
    session: &std::sync::Arc<crate::quic::server::Session>, requests: Vec<Request>,
) -> Result<Vec<Response>> {
    let mut pending = Vec::new();
    for request in requests {
        let session = session.clone();
        pending.push(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            rpc(&session, request).await
        })));
    }
    let mut responses = Vec::new();
    // Await all tasks even if one fails, so an error doesn't leave detached network work.
    let mut error = None;
    for task in pending {
        match task.await {
            Ok(Ok(reply)) => responses.push(reply),
            Ok(Err(e)) => error = Some(e),
            Err(e) => error = Some(e.into()),
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(responses)
}

#[derive(Serialize, Deserialize)]
enum Content {
    Name { name: String, card: Vec<u8> },
    Bio(String),
    Avatar(Option<Vec<u8>>),
}
fn own_content(identity: &Identity, field: Field) -> Content {
    match field {
        Field::Name => {
            let d = identity.details();
            Content::Name { name: d.name, card: d.card }
        },
        Field::Bio => Content::Bio(identity.details().bio),
        Field::Avatar => Content::Avatar(identity.avatar()),
    }
}
fn staged(
    conn: &Connection, owner: &[u8; 32], field: Field,
) -> Result<Option<(Vec<u8>, Publication)>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = one(
        conn,
        "SELECT fingerprint, publication FROM profile_publication WHERE owner=?1 AND field=?2",
        (owner.as_slice(), field.id()),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    row.map(|(fingerprint, bytes)| Ok((fingerprint, Publication::deser(&bytes)?))).transpose()
}

/// Returns true once this identity uses the store; errors must never downgrade to queued MLS.
pub(super) async fn reconcile() -> Result<bool> {
    let Some(session) = core().session() else { return Ok(false) };
    let Some(identity) = Identity::get() else { return Ok(false) };
    let owner = identity.ipk();
    let pinned = host(&owner)?;
    if !session.services().await.supports(services::PROFILE_STORE, services::PROFILE_STORE_VERSION)
    {
        ensure!(pinned.is_none(), "profile storage relay unavailable; publication deferred");
        return Ok(false);
    }
    ensure!(
        pinned.as_ref().is_none_or(|h| h.as_str() == session.relay.id.as_ref()),
        "profile storage is on another relay; migration is not automatic"
    );
    // Pin before any upload. A lost ACK or connection switch must not create an unmanaged copy.
    if pinned.is_none() {
        crate::data::app_prefs::set(
            &format!("profile-host:{}", hex::encode(owner)),
            &session.relay.id,
        )?;
    }
    let signer = crate::data::identity::secret_key_signing(&owner)?;
    session
        .profile_registered
        .get_or_try_init(|| async {
            ensure!(
                rpc(&session, Request::Register(crypto::reader_key(&signer)?)).await?
                    == Response::Accepted,
                "profile registration rejected"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    let peers: Vec<_> = Contact::list()
        .into_iter()
        .filter(|p| p.status == PAIR_STATUS_PAIRED)
        .filter(|p| !crate::requests::is_blocked(&p.ipk))
        .map(|p| p.ipk)
        .collect();
    let mut keys = BTreeMap::new();
    // Missing keys are expected during upgrade. Do not enqueue probes to those peers.
    for chunk in peers.chunks(8) {
        let requests =
            chunk.iter().map(|peer| Request::ReaderKey { owner: (*peer).into() }).collect();
        for (peer, reply) in chunk.iter().zip(batch(&session, requests).await?) {
            match reply {
                Response::ReaderKey(Some(key)) => {
                    if key.owner.0 == *peer
                        && common::crypto::verify_ed25519(peer, &key.input(), &key.signature.0)
                            .is_ok()
                    {
                        keys.insert(*peer, key);
                    } else {
                        log::warn!("PROFILE: ignored an unauthenticated reader key");
                    }
                },
                Response::ReaderKey(None) => {},
                _ => bail!("profile key fetch failed"),
            }
        }
    }
    let mut failure = None;
    for field in Field::ALL {
        let result = async {
        let identity = Identity::get().ok_or_else(|| anyhow!("identity unavailable"))?;
        ensure!(identity.ipk() == owner, "identity changed during profile reconciliation");
        let sharing = policy(field)?;
        let readers: Vec<_> = keys
            .iter()
            .filter(|(peer, _)| {
                sharing.accepted_contacts
                    && !sharing.excluded.iter().any(|p| p.as_slice() == peer.as_slice())
                    && Contact::is_paired(peer)
                    && !crate::requests::is_blocked(peer)
            })
            .map(|(_, key)| key.clone())
            .collect();
        ensure!(readers.len() <= MAX_READERS, "too many readers for this profile field");
        // An empty audience is a small durable tombstone, not an encrypted copy of withdrawn data.
        let plain =
            if readers.is_empty() { Vec::new() } else { own_content(&identity, field).ser()? };
        let fingerprint = blake3::hash(&(plain.as_slice(), &readers).ser()?).as_bytes().to_vec();
        let Response::Head(remote) = rpc(&session, Request::Head { field }).await? else {
            bail!("profile head fetch failed")
        };
        let old = staged(&core().db.network().lock(), &owner, field)?;
        let publication = match old {
            Some((hash, publication))
                if hash == fingerprint
                    && remote.as_ref().is_none_or(|(object, version)| {
                        *object == publication.value.object && *version <= publication.value.version
                    }) =>
            {
                publication
            },
            old => {
                let previous = old.as_ref().map(|(_, p)| &p.value);
                let object = remote
                    .as_ref()
                    .map(|(o, _)| o.0)
                    .or_else(|| previous.map(|p| p.object.0))
                    .unwrap_or_else(get_nonce);
                let version = remote
                    .as_ref()
                    .map_or(0, |(_, v)| *v)
                    .max(previous.map_or(0, |p| p.version))
                    .max(now_ms())
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("profile version exhausted"))?;
                ensure!(version <= i64::MAX as u64, "profile version exhausted");
                let publication = crypto::seal(&signer, field, object, version, &plain, &readers)?;
                core().db.network().lock().execute("INSERT INTO profile_publication(owner,field,fingerprint,publication) VALUES (?1,?2,?3,?4)
                    ON CONFLICT(owner,field) DO UPDATE SET fingerprint=excluded.fingerprint,publication=excluded.publication",
                    (owner.as_slice(), field.id(), &fingerprint, publication.ser()?))?;
                publication
            },
        };
        if remote != Some((publication.value.object, publication.value.version)) {
            ensure!(
                rpc(&session, Request::Publish(publication)).await? == Response::Accepted,
                "profile publication rejected"
            );
        }
        Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = result {
            failure = Some(error);
        }
    }
    let fetched = fetch(&session, &signer, &peers).await;
    if let Some(error) = failure {
        return Err(error);
    }
    fetched?;
    Ok(true)
}

pub(super) async fn refresh_pending() -> Result<()> {
    let _update = core().profile_update.lock().await;
    let peers = std::mem::take(&mut *core().profile_refresh.lock())
        .into_iter()
        .filter(|p| Contact::is_paired(p) && !crate::requests::is_blocked(p))
        .collect::<Vec<_>>();
    if peers.is_empty() {
        return Ok(());
    }
    let session = core().session().ok_or_else(|| anyhow!("profile relay unavailable"))?;
    let owner = Identity::local_ipk().ok_or_else(|| anyhow!("identity unavailable"))?;
    ensure!(
        host(&owner)?.is_some_and(|h| h.as_str() == session.relay.id.as_ref()),
        "profile storage relay unavailable"
    );
    let signer = crate::data::identity::secret_key_signing(&owner)?;
    fetch(&session, &signer, &peers).await
}

async fn fetch(
    session: &std::sync::Arc<crate::quic::server::Session>, signer: &ed25519_dalek::SigningKey,
    peers: &[[u8; 32]],
) -> Result<()> {
    for field in Field::ALL {
        for chunk in peers.chunks(8) {
            let requests = chunk
                .iter()
                .map(|peer| -> Result<_> {
                    let known = cached(&core().db.messages().lock(), peer, field)?;
                    Ok(Request::Fetch {
                        owner: (*peer).into(),
                        field,
                        known_version: known.map(|(_, v, _)| v),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            for (&peer, reply) in chunk.iter().zip(batch(&session, requests).await?) {
                let received = (|| -> Result<bool> {
                    let (value, content) = match reply {
                        Response::Value { value, grant } => {
                            crypto::verify(&value, &peer, field)?;
                            let bytes = crypto::open(&signer, &value, &grant)?;
                            (value, Some(Content::deser(&bytes)?))
                        },
                        Response::Withdrawn { value } => {
                            crypto::verify(&value, &peer, field)?;
                            (value, None)
                        },
                        Response::Unchanged | Response::Missing => return Ok(false),
                        _ => bail!("profile fetch failed"),
                    };
                    apply(&core().db.messages().lock(), &value, content)
                })();
                match received {
                    Ok(true) => {
                        crate::data::peer_avatar::notify_changed();
                        crate::data::peer_profile::notify_changed();
                    },
                    Ok(false) => {},
                    Err(error) => log::warn!("PROFILE: rejected a field response: {error}"),
                }
            }
        }
    }
    Ok(())
}

fn cached(
    conn: &Connection, owner: &[u8; 32], field: Field,
) -> Result<Option<([u8; 32], u64, bool)>> {
    Ok(one(
        conn,
        "SELECT object,version,withdrawn FROM profile_fields WHERE owner=?1 AND field=?2",
        (owner.as_slice(), field.id()),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}
pub(crate) fn has_field(conn: &Connection, owner: &[u8; 32], field: Field) -> bool {
    // Fail closed on a storage error: legacy controls must not bypass a withdrawal.
    cached(conn, owner, field).map_or(true, |row| row.is_some())
}
fn apply(conn: &Connection, value: &Value, content: Option<Content>) -> Result<bool> {
    let owner = &value.owner.0;
    let tx = conn.unchecked_transaction()?;
    if let Some((object, version, withdrawn)) = cached(&tx, owner, value.field)? {
        ensure!(object == value.object.0, "profile object changed without migration");
        if value.version < version || (value.version == version && (withdrawn || content.is_some()))
        {
            return Ok(false);
        }
    }
    let withdrawn = content.is_none();
    let content = content.unwrap_or_else(|| match value.field {
        Field::Name => Content::Name { name: String::new(), card: vec![] },
        Field::Bio => Content::Bio(String::new()),
        Field::Avatar => Content::Avatar(None),
    });
    match (value.field, content) {
        (Field::Name, Content::Name { name, card }) => {
            ensure!(
                (withdrawn || !name.trim().is_empty()) && name.chars().count() <= 32,
                "invalid profile name"
            );
            if !card.is_empty() {
                let c = crate::contact_card::verify_card(&card)?;
                ensure!(c.ipk == *owner && c.name == name, "profile card mismatch");
            }
            tx.execute(
                "INSERT INTO peer_profiles(ipk,name,bio,revision,card) VALUES (?1,?2,'',0,?3)
                ON CONFLICT(ipk) DO UPDATE SET name=excluded.name,card=excluded.card",
                (owner.as_slice(), name, card),
            )?;
            tx.execute("DELETE FROM peer_names WHERE ipk=?1", [owner.as_slice()])?;
        },
        (Field::Bio, Content::Bio(bio)) => {
            ensure!(bio.chars().count() <= 160, "invalid profile bio");
            tx.execute(
                "INSERT INTO peer_profiles(ipk,name,bio,revision,card) VALUES (?1,'',?2,0,X'')
                ON CONFLICT(ipk) DO UPDATE SET bio=excluded.bio",
                (owner.as_slice(), bio),
            )?;
        },
        (Field::Avatar, Content::Avatar(avif)) => {
            if let Some(bytes) = &avif {
                crate::data::peer_avatar::check_avif(bytes)?;
            }
            tx.execute(
                "INSERT INTO peer_avatars(ipk,avif,updated_at,revision) VALUES (?1,?2,?3,0)
                ON CONFLICT(ipk) DO UPDATE SET avif=excluded.avif,updated_at=excluded.updated_at",
                (owner.as_slice(), avif, now_ms() / 1000),
            )?;
        },
        _ => bail!("profile field/content mismatch"),
    }
    tx.execute("INSERT INTO profile_fields(owner,field,object,version,withdrawn) VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(owner,field) DO UPDATE SET version=excluded.version,withdrawn=excluded.withdrawn",
        (owner.as_slice(),value.field.id(),value.object.0.as_slice(),value.version,withdrawn))?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn value(field: Field, version: u64) -> Value {
        Value {
            owner: [1; 32].into(),
            object: [field.id(); 32].into(),
            field,
            version,
            ciphertext: vec![].into(),
            signature: [0; 64].into(),
        }
    }
    #[test]
    fn fields_withdraw_independently_and_failed_transactions_leave_no_high_watermark() {
        let conn = crate::test_support::data::open(crate::db::messages::migrate);
        let name = || Some(Content::Name { name: "Alice".into(), card: vec![] });
        assert!(apply(&conn, &value(Field::Name, 1), name()).unwrap());
        assert!(apply(&conn, &value(Field::Bio, 20), Some(Content::Bio("hello".into()))).unwrap());
        assert!(apply(&conn, &value(Field::Name, 2), None).unwrap());
        assert!(!apply(&conn, &value(Field::Name, 1), name()).unwrap());
        assert!(!apply(&conn, &value(Field::Name, 2), name()).unwrap());
        let (n, b): (String, String) = conn
            .query_row("SELECT name,bio FROM peer_profiles", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((n, b), ("".into(), "hello".into()));
        assert!(has_field(&conn, &[1; 32], Field::Name));
        conn.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY);
            CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
            CREATE TRIGGER fail_commit AFTER UPDATE ON peer_profiles BEGIN INSERT INTO child VALUES(1); END;").unwrap();
        assert!(apply(&conn, &value(Field::Name, 3), name()).is_err());
        assert_eq!(cached(&conn, &[1; 32], Field::Name).unwrap().unwrap().1, 2);
        conn.execute_batch("DROP TRIGGER fail_commit;").unwrap();
        assert!(apply(&conn, &value(Field::Name, 3), name()).unwrap());
        let mut other = value(Field::Name, 4);
        other.object = [99; 32].into();
        assert!(apply(&conn, &other, name()).is_err());
        assert!(
            apply(&conn, &value(Field::Avatar, 1), Some(Content::Bio("wrong".into()))).is_err()
        );
        assert!(cached(&conn, &[1; 32], Field::Avatar).unwrap().is_none());
    }

    #[tokio::test]
    async fn reconciliation_recovers_lost_ack_coalesces_edits_and_never_dispatches() {
        use crate::test_support::{ScopedCore, data, net};
        use ed25519_dalek::SigningKey;
        use std::sync::Arc;
        let scope = ScopedCore::new();
        let me = data::identity(&scope.core.db.identity().lock(), 71);
        let peer = SigningKey::from_bytes(&[72; 32]);
        let peer_id = peer.verifying_key().to_bytes();
        Contact::save(peer_id, "Peer".into()).unwrap();
        let their_name = crypto::seal(
            &peer,
            Field::Name,
            [73; 32],
            1,
            &Content::Name { name: "Peer name".into(), card: vec![] }.ser().unwrap(),
            &[crypto::reader_key(&me).unwrap()],
        )
        .unwrap();
        let peer_key = crypto::reader_key(&peer).unwrap();
        let publications = Arc::new(parking_lot::Mutex::new(BTreeMap::<u8, Publication>::new()));
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ours, theirs) = net::connection().await;
        let session = Arc::new(net::session(ours));
        scope.core.publish(session);
        let task = tokio::spawn({
            let publications = publications.clone();
            let sent = sent.clone();
            async move {
                let mut lose_ack = true;
                while let Ok((mut tx, mut rx)) = theirs.accept_bi().await {
                    let packet = CRelayPacket::unpack(&mut rx).await.unwrap();
                    let response = match packet {
                        CRelayPacket::ServiceCapabilities => {
                            let support = common::contracts::Support::new([(
                                services::PROFILE_STORE,
                                vec![1],
                            )])
                            .unwrap();
                            SRelayPacket::ServiceCapabilities { supported: support.encode().into() }
                        },
                        CRelayPacket::Profile(request) => SRelayPacket::Profile(match request {
                            Request::Register(_) => Response::Accepted,
                            Request::ReaderKey { owner } => {
                                assert_eq!(owner.0, peer_id);
                                Response::ReaderKey(Some(peer_key.clone()))
                            },
                            Request::Head { field } => Response::Head(
                                publications
                                    .lock()
                                    .get(&field.id())
                                    .map(|p| (p.value.object, p.value.version)),
                            ),
                            Request::Publish(p) => {
                                sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                publications.lock().insert(p.value.field.id(), p);
                                if lose_ack {
                                    lose_ack = false;
                                    tx.reset(0u32.into()).unwrap();
                                    continue;
                                }
                                Response::Accepted
                            },
                            Request::Fetch { owner, field, known_version } => {
                                assert_eq!(owner.0, peer_id);
                                if field != Field::Name {
                                    Response::Missing
                                } else if known_version == Some(1) {
                                    Response::Unchanged
                                } else {
                                    Response::Value {
                                        value: their_name.value.clone(),
                                        grant: their_name.grants[0].clone(),
                                    }
                                }
                            },
                        }),
                        other => panic!("profile reconciliation must not dispatch: {other:?}"),
                    };
                    response.send(&mut tx).await.unwrap();
                    tx.finish().unwrap();
                }
            }
        });
        assert!(reconcile().await.is_err(), "first publication loses its ACK after storage");
        let first = publications.lock()[&Field::Name.id()].clone();
        assert!(reconcile().await.unwrap());
        assert_eq!(
            publications.lock()[&Field::Name.id()],
            first,
            "retry preserves accepted bytes and version"
        );
        assert_eq!(sent.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(crate::data::peer_profile::get(&peer_id).unwrap().name, "Peer name");
        assert!(reconcile().await.unwrap());
        assert_eq!(sent.load(std::sync::atomic::Ordering::Relaxed), 3, "no unchanged uploads");
        Identity::set_details("First edit", "").unwrap();
        Identity::set_details("Latest edit", "").unwrap();
        assert!(reconcile().await.unwrap());
        assert_eq!(
            sent.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "only the changed name is published"
        );
        let latest = publications.lock()[&Field::Name.id()].clone();
        let content =
            Content::deser(&crypto::open(&peer, &latest.value, &latest.grants[0]).unwrap())
                .unwrap();
        assert!(matches!(content,Content::Name { name,.. } if name == "Latest edit"));
        set_profile_sharing(
            ProfileField::Name,
            ProfileSharing { accepted_contacts: false, excluded: vec![] },
        )
        .unwrap();
        assert!(reconcile().await.unwrap());
        assert!(publications.lock()[&Field::Name.id()].grants.is_empty());
        assert_eq!(
            publications.lock()[&Field::Bio.id()].grants.len(),
            1,
            "bio policy remains independent"
        );
        Contact::delete(&peer_id).unwrap();
        assert!(reconcile().await.unwrap());
        assert!(publications.lock().values().all(|p| p.grants.is_empty()));
        task.abort();
    }
}
