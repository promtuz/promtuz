//! Private MLS persistence. The public mirror is local to the MLS connection so transport storage
//! never acquires the messages mutex. Locked chats retain only code-protected state and ciphertext
//! inbox entries; no MLS key capable of reading old traffic remains in readable storage.
use crate::state::core;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

fn metadata(conn: &Connection, key: &[u8], storage: bool) -> Result<Option<([u8; 16], Vec<u8>)>> {
    let sql = if storage {
        "SELECT conversation_id,public_key FROM mls_private_conversations WHERE storage_key=?1"
    } else {
        "SELECT conversation_id,public_key FROM mls_private_conversations WHERE group_id=?1"
    };
    Ok(conn.query_row(sql, [key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
}
pub(crate) fn seal_storage(conn: &Connection, key: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    seal(conn, key, bytes, true)
}
pub(crate) fn open_storage(conn: &Connection, key: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    open(conn, key, bytes, true)
}
pub(crate) fn seal_payload(conn: &Connection, gid: &[u8; 32], bytes: &[u8]) -> Result<Vec<u8>> {
    seal(conn, gid, bytes, false)
}
pub(crate) fn open_payload(conn: &Connection, gid: &[u8; 32], bytes: &[u8]) -> Result<Vec<u8>> {
    open(conn, gid, bytes, false)
}
fn seal(conn: &Connection, key: &[u8], bytes: &[u8], storage: bool) -> Result<Vec<u8>> {
    let Some((conv, public)) = metadata(conn, key, storage)? else { return Ok(bytes.to_vec()) };
    crate::privacy::seal_with_public(&conv, &public, bytes)
}
fn open(conn: &Connection, key: &[u8], bytes: &[u8], storage: bool) -> Result<Vec<u8>> {
    let Some((conv, _)) = metadata(conn, key, storage)? else { return Ok(bytes.to_vec()) };
    crate::privacy::open_bytes(&conv, bytes)
}
fn sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(b"pz-private-v1:")
}

/// Caller holds the group's operation lock. The messages DB already owns the code-wrapped key.
/// Publish the mirror before conversion: a failed conversion then blocks use instead of allowing
/// a late transport operation to write plaintext. Startup retries before opening network sessions.
pub(crate) fn install(
    gid: &[u8; 32], conv: &[u8; 16], public: &[u8], salt: &[u8], wrapped: &[u8],
) -> Result<()> {
    let connection = core().db.mls();
    let mut conn = connection.lock();
    let mut key = Vec::new();
    ciborium::ser::into_writer(&openmls::prelude::GroupId::from_slice(gid), &mut key)?;
    conn.pragma_update(None, "secure_delete", "ON")?;
    conn.execute("INSERT INTO mls_private_conversations(group_id,storage_key,conversation_id,public_key,salt,wrapped_key)
        VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(group_id) DO NOTHING",params![gid,key,conv,public,salt,wrapped])?;
    let saved: ([u8; 16], Vec<u8>) = metadata(&conn, gid, false)?.expect("private mirror inserted");
    ensure!(saved.0 == *conv && saved.1 == public, "private MLS metadata conflict");
    if conn.query_row(
        "SELECT complete FROM mls_private_conversations WHERE group_id=?1",
        [gid],
        |r| r.get::<_, u32>(0),
    )? >= 2 {
        return Ok(());
    }
    let tx = conn.transaction()?;
    let rows = crate::db::all(
        &tx,
        "SELECT key_tag,sub_key,value FROM mls_storage WHERE group_id=?1",
        [&key],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)),
    )?;
    for (tag, sub, value) in rows {
        if !sealed(&value) {
            tx.execute(
                "UPDATE mls_storage SET value=?4 WHERE group_id=?1 AND key_tag=?2 AND sub_key=?3",
                params![key, tag, sub, seal_storage(&tx, &key, &value)?],
            )?;
        }
    }
    super::recovery::privatize_snapshots(&tx, gid, &key)?;
    // Signed proofs carry historical names, roles and changes. They have no transport
    // secrets, but must share the history code boundary rather than becoming a readable log.
    for (table, column) in [("mls_replay", "payload"), ("mls_group_received", "payload"),
        ("mls_branches", "change_blob"), ("mls_branches", "proof"), ("mls_join_history", "history")] {
        let rows = crate::db::all(
            &tx,
            &format!("SELECT rowid,{column} FROM {table} WHERE group_id=?1 AND length({column})>0"),
            [gid],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        for (row, value) in rows {
            if !sealed(&value) {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=?2 WHERE rowid=?1"),
                    params![row, seal_payload(&tx, gid, &value)?],
                )?;
            }
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO mls_group_size(group_id,total_bytes)
        SELECT ?1,coalesce(sum(length(value)),0) FROM mls_storage WHERE group_id=?1",
        [&key],
    )?;
    tx.commit()?;
    conn.execute_batch(
        "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    conn.execute("UPDATE mls_private_conversations SET complete=2 WHERE group_id=?1", [gid])?;
    Ok(())
}

/// Inverse of `install`, for removing a lock. Caller holds the operation lock and an open session.
/// Startup repair reinstalls the mirror if the messages side never commits.
pub(crate) fn uninstall(gid: &[u8; 32]) -> Result<()> {
    let connection = core().db.mls();
    let mut conn = connection.lock();
    if metadata(&conn, gid, false)?.is_none() {
        return Ok(());
    }
    ensure!(
        !conn.query_row("SELECT EXISTS(SELECT 1 FROM mls_private_inbox WHERE group_id=?1)", [gid], |r| r.get::<_, bool>(0))?,
        "Messages are still arriving. Try again in a moment."
    );
    let mut key = Vec::new();
    ciborium::ser::into_writer(&openmls::prelude::GroupId::from_slice(gid), &mut key)?;
    let tx = conn.transaction()?;
    let rows = crate::db::all(
        &tx,
        "SELECT key_tag,sub_key,value FROM mls_storage WHERE group_id=?1",
        [&key],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)),
    )?;
    for (tag, sub, value) in rows {
        if sealed(&value) {
            tx.execute(
                "UPDATE mls_storage SET value=?4 WHERE group_id=?1 AND key_tag=?2 AND sub_key=?3",
                params![key, tag, sub, open_storage(&tx, &key, &value)?],
            )?;
        }
    }
    super::recovery::publicize_snapshots(&tx, gid, &key)?;
    for (table, column) in [("mls_replay", "payload"), ("mls_group_received", "payload"),
        ("mls_branches", "change_blob"), ("mls_branches", "proof"), ("mls_join_history", "history")] {
        let rows = crate::db::all(
            &tx,
            &format!("SELECT rowid,{column} FROM {table} WHERE group_id=?1 AND length({column})>0"),
            [gid],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        for (row, value) in rows {
            if sealed(&value) {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=?2 WHERE rowid=?1"),
                    params![row, open_payload(&tx, gid, &value)?],
                )?;
            }
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO mls_group_size(group_id,total_bytes)
        SELECT ?1,coalesce(sum(length(value)),0) FROM mls_storage WHERE group_id=?1",
        [&key],
    )?;
    tx.execute("DELETE FROM mls_private_conversations WHERE group_id=?1", [gid])?;
    tx.commit()?;
    Ok(())
}

/// Discard inaccessible history and transport keys together. Caller must hold operation_lock and
/// arrange a fresh session with the existing contact; never leave encrypted rows under normal
/// metadata, where the decoder might mistake them for readable state.
pub(crate) fn clear_private_group_tx(conn: &Connection, gid: &[u8; 32]) -> Result<()> {
    let key: Option<Vec<u8>> = conn
        .query_row(
            "SELECT storage_key FROM mls_private_conversations WHERE group_id=?1",
            [gid],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(key) = key {
        conn.execute("DELETE FROM mls_storage WHERE group_id=?1", [&key])?;
        conn.execute("DELETE FROM mls_group_size WHERE group_id=?1", [&key])?;
        for table in [
            "mls_branches",
            "mls_recovery_roots",
            "mls_replay",
            "mls_dispatch_jobs",
            "mls_dispatch_ids",
            "mls_branch_inbox",
            "mls_group_received",
            "mls_join_history",
            "mls_recovery_retries",
            "mls_migration_consents",
            "mls_epoch_ahead",
            "mls_private_inbox",
            "mls_private_conversations",
        ] {
            conn.execute(&format!("DELETE FROM {table} WHERE group_id=?1"), [gid])?;
        }
        conn.execute("DELETE FROM mls_group_migrations WHERE group_id=?1 OR target=?1", [gid])?;
    }
    Ok(())
}

fn envelope_group(envelope: &common::proto::mls_wire::MlsEnvelopeP) -> Option<[u8; 32]> {
    use common::proto::mls_wire::MlsEnvelopeP::*;
    match envelope {
        Application(e) | GroupApplication { message: e, .. } => Some(e.group_id.0),
        Welcome(e) | GroupWelcome { welcome: e, .. } => Some(e.group_id.0),
        GroupMemberRequest { group, .. }
        | GroupMigrationReady { group, .. }
        | GroupMigrationWelcome { group, .. } => Some(group.0),
        _ => None,
    }
}
/// Associate a replacement direct session before any Welcome/MLS decrypt can persist keys.
/// Group variants are deliberately excluded: sharing a peer does not make their other groups
/// private. Inspect messages first, release that mutex, then acquire the MLS operation lock.
pub(crate) fn prepare_direct_envelope(
    provider: &super::PromtuzMlsProvider, envelope: &common::proto::mls_wire::MlsEnvelopeP,
    sender: &[u8; 32],
) -> Result<()> {
    use common::proto::mls_wire::MlsEnvelopeP;
    if !core().db.owns_mls_connection(&provider.storage().connection()) {
        return Ok(());
    }
    let gid = match envelope {
        MlsEnvelopeP::Application(e) => e.group_id.0,
        MlsEnvelopeP::Welcome(e) if e.sender_ipk.0 == *sender => e.group_id.0,
        _ => return Ok(()),
    };
    let _configuration = core().private_configuration.lock();
    let private: Option<([u8; 16], Vec<u8>, Vec<u8>, Vec<u8>)> = {
        let conn = core().db.messages().lock();
        conn.query_row("SELECT p.conversation_id,p.public_key,p.salt,p.wrapped_key
            FROM private_conversations p JOIN conversations c ON c.id=p.conversation_id
            JOIN conversation_members m ON m.conversation_id=c.id
            WHERE c.kind=0 AND m.member_ipk=?1
              AND NOT EXISTS(SELECT 1 FROM conversations other WHERE other.mls_group_id=?2 AND other.id<>c.id)",
            params![sender,gid],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?
    };
    // A sealed hidden chat has no roster row; its marker recognizes the person instead.
    let private = match private {
        Some(found) => Some(found),
        None => match crate::hidden::concealed_sender(sender) {
            Some((_, conv)) => core().db.messages().lock().query_row(
                "SELECT p.conversation_id,p.public_key,p.salt,p.wrapped_key FROM private_conversations p
                WHERE p.conversation_id=?1
                  AND NOT EXISTS(SELECT 1 FROM conversations other WHERE other.mls_group_id=?2 AND other.id<>?1)",
                params![conv, gid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?,
            None => None,
        },
    };
    if let Some((conv, public, salt, wrapped)) = private {
        // A relay must not be able to invent replacement private sessions. Authenticate the
        // public envelope before installing metadata or doing an expensive database rewrite.
        let transcript = match envelope {
            MlsEnvelopeP::Welcome(e) => common::proto::mls_wire::welcome_envelope_signing_input(
                common::proto::mls_wire::MLS_WIRE_VERSION,
                &e.group_id.0,
                &e.sender_ipk.0,
                &e.recipient_ipk.0,
                &e.kp_ref_used.0,
                &e.welcome_blob.0,
            ),
            MlsEnvelopeP::Application(e) => {
                let ours = crate::data::identity::Identity::local_ipk()
                    .ok_or_else(|| anyhow::anyhow!("identity not found"))?;
                common::proto::mls_wire::envelope_signing_input(
                    common::PROTOCOL_VERSION,
                    &ours,
                    &e.group_id.0,
                    e.epoch,
                    &e.mls_message.0,
                )
            },
            _ => unreachable!(),
        };
        let signature = match envelope {
            MlsEnvelopeP::Welcome(e) => &e.sender_sig.0,
            MlsEnvelopeP::Application(e) => &e.sender_sig.0,
            _ => unreachable!(),
        };
        common::crypto::verify_ed25519(sender, &transcript, signature)
            .map_err(|_| anyhow::anyhow!("private session envelope signature failed"))?;
        let _operation = super::recovery::operation_lock(&gid).lock();
        install(&gid, &conv, &public, &salt, &wrapped)?;
    }
    Ok(())
}

/// Install protection before creating a replacement outgoing direct session. This must run
/// before OpenMLS writes its first value, including on a restore with no bound group.
pub(crate) fn prepare_outgoing_direct(
    provider: &super::PromtuzMlsProvider, peer: &[u8; 32], gid: &[u8; 32],
) -> Result<()> {
    if !core().db.owns_mls_connection(&provider.storage().connection()) {
        return Ok(());
    }
    let _configuration = core().private_configuration.lock();
    let private: Option<([u8; 16], Vec<u8>, Vec<u8>, Vec<u8>)> = {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT p.conversation_id,p.public_key,p.salt,p.wrapped_key
            FROM private_conversations p JOIN conversations c ON c.id=p.conversation_id
            JOIN conversation_members m ON m.conversation_id=c.id
            WHERE c.kind=0 AND m.member_ipk=?1",
            [peer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
    };
    // Someone hidden is unreachable from any other chat until their own chat is unlocked.
    ensure!(private.is_some() || crate::hidden::concealed_sender(peer).is_none(), "peer is unreachable");
    if let Some((conv, public, salt, wrapped)) = private {
        let _operation = super::recovery::operation_lock(gid).lock();
        ensure!(core().private_sessions.read().contains_key(&conv), "Chat is locked");
        install(gid, &conv, &public, &salt, &wrapped)?;
    }
    Ok(())
}

/// Only called on authenticated transport dispatches. The inner MLS signature and authorization
/// are rechecked when draining. A full inbox is a storage failure so delivery stays unacknowledged.
pub(crate) fn maybe_buffer(
    provider: &super::PromtuzMlsProvider, envelope: &common::proto::mls_wire::MlsEnvelopeP,
    sender: &[u8; 32], payload: &[u8], accepted: u64, id: &[u8; 16],
) -> Result<bool> {
    let Some(gid) = envelope_group(envelope) else { return Ok(false) };
    if !provider.storage().with_conn(|conn| metadata(conn, &gid, false))?.is_some() {
        return Ok(false);
    }
    let concealed = crate::hidden::concealed_sender(sender);
    let _operation = super::recovery::operation_lock(&gid).lock();
    provider.storage().with_tx(|conn| {
        let Some((conv,public))=metadata(conn,&gid,false)? else{return Ok(false)};
        if core().private_sessions.read().contains_key(&conv) {return Ok(false)}
        let (key,hidden)=match &concealed {
            Some((marker,c)) if *c==conv => (*marker,true),
            _ => (*sender,false),
        };
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM mls_private_inbox WHERE group_id=?1 AND sender=?2 AND dispatch_id=?3)",params![gid,key,id],|r|r.get(0))?;
        if exists{return Ok(true)}
        let (count,bytes):(u64,u64)=conn.query_row("SELECT count(*),coalesce(sum(length(envelope)),0) FROM mls_private_inbox WHERE group_id=?1",[gid],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if count>=4096 || bytes.saturating_add(payload.len() as u64)>64*1024*1024 {
            return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),Some("private inbox is full".into())).into());
        }
        // A replacement Welcome may still have an unconsumed, ordinary KeyPackage. Wrap the
        // entire envelope in the code boundary as well, so that package cannot bypass the code.
        let mut protected=crate::privacy::seal_with_public(&conv,&public,payload)?;
        if hidden {
            protected=crate::privacy::seal_with_public(&conv,&public,&postcard::to_allocvec(&(sender,protected))?)?;
        }
        conn.execute("INSERT INTO mls_private_inbox(group_id,sender,dispatch_id,accepted_at_ms,envelope,hidden) VALUES(?1,?2,?3,?4,?5,?6)",params![gid,key,id,accepted,protected,hidden])?;
        Ok(true)
    })
}

pub(crate) async fn drain(conv: [u8; 16]) -> Result<()> {
    let Some(session) = core().session() else { return Ok(()) };
    let provider = super::PromtuzMlsProvider::shared();
    let db = core().db.mls();
    let stash = super::KeyPackageStash::new(db.clone());
    let buffer = super::EpochCatchupBuffer::new(db);
    let ctx = crate::messaging::session::MlsContext {
        provider: &provider,
        stash: &stash,
        buffer: &buffer,
        dht: session.dht.as_ref(),
    };
    drain_with(&ctx, conv).await?;
    if crate::privacy::require_access(&conv).is_ok() { crate::groups::resume(conv); }
    Ok(())
}

async fn drain_with<C: crate::quic::dht_client::DhtClient>(
    ctx: &crate::messaging::session::MlsContext<'_, C>, conv: [u8; 16],
) -> Result<()> {
    let generation=core().private_generation.load(std::sync::atomic::Ordering::SeqCst);
    drain_generation(ctx,conv,generation).await
}
async fn drain_generation<C: crate::quic::dht_client::DhtClient>(
    ctx: &crate::messaging::session::MlsContext<'_, C>, conv: [u8; 16], generation:u64,
) -> Result<()> {
    let provider = ctx.provider;
    let current=|| {
        let sessions=core().private_sessions.read();
        generation==core().private_generation.load(std::sync::atomic::Ordering::SeqCst)
            && sessions.contains_key(&conv)
    };
    let rows=provider.storage().with_conn(|conn| -> Result<Vec<(i64,[u8;32],[u8;16],u64,Vec<u8>,bool)>> {
        Ok(crate::db::all(conn,"SELECT i.rowid,i.sender,i.dispatch_id,i.accepted_at_ms,i.envelope,i.hidden FROM mls_private_inbox i JOIN mls_private_conversations p ON p.group_id=i.group_id WHERE p.conversation_id=?1 ORDER BY i.rowid",[conv],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?)
    })?;
    for (row, sender, id, accepted, payload, hidden) in rows {
        if !current() {
            break;
        }
        let (sender, payload) = if hidden {
            match crate::privacy::open_bytes(&conv, &payload) {
                Ok(inner) => postcard::from_bytes::<([u8; 32], Vec<u8>)>(&inner)?,
                Err(_) if !current() => break,
                Err(error) => return Err(error),
            }
        } else {
            (sender, payload)
        };
        let payload = match crate::privacy::open_bytes(&conv, &payload) {
            Ok(payload) => zeroize::Zeroizing::new(payload),
            Err(_) if !current() => break,
            Err(error) => return Err(error),
        };
        let result = crate::messaging::receive::process_inbound_envelope(
            ctx, sender, &payload, accepted, id,
        )
        .await;
        // A lock interruption re-buffers the same identity. Keep the row, regardless of result.
        if !current() {
            break;
        }
        if let Err(error) = result {
            // The relay has already acknowledged this durable inbox entry. A rejected inner
            // signature or malformed payload cannot improve on retry and must not poison every
            // later unlock. Storage failures still retain the row for a later drain; relocking
            // was handled above before classifying any failure.
            if crate::utils::is_storage_error(&error) {
                return Err(error);
            }
            log::debug!("PRIVATE: discarded a rejected buffered envelope");
        }
        provider.storage().with_conn(|conn| {
            // The database then capability lock order matches storage opens. Keep the final
            // validation and deletion together so rapid relocking cannot acknowledge old work.
            let sessions=core().private_sessions.read();
            if generation!=core().private_generation.load(std::sync::atomic::Ordering::SeqCst)
                || !sessions.contains_key(&conv) { return Ok(0); }
            conn.execute("DELETE FROM mls_private_inbox WHERE rowid=?1", [row])
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mls::PromtuzMlsProvider;
    use common::proto::pack::Packer;
    #[tokio::test]
    async fn private_mls_conversion_seals_live_archive_and_journal_and_buffers_ciphertext() {
        let scope = crate::test_support::ScopedCore::new();
        let (conv, gid, branch) = ([111u8; 16], [112u8; 32], [113u8; 32]);
        scope
            .core
            .db
            .messages()
            .lock()
            .execute("INSERT INTO conversations(id,mls_group_id) VALUES(?1,?2)", params![conv, gid])
            .unwrap();
        let mut key = Vec::new();
        ciborium::ser::into_writer(&openmls::prelude::GroupId::from_slice(&gid), &mut key).unwrap();
        let secret = b"old-ratchet-secret-never-readable".to_vec();
        let snapshot = lz4_flex::compress_prepend_size(
            &postcard::to_allocvec(&vec![(10i64, Vec::<u8>::new(), secret.clone())]).unwrap(),
        );
        let mls = scope.core.db.mls();
        {
            let db = mls.lock();
            db.execute("INSERT INTO mls_storage VALUES(?1,10,X'',?2)", params![key, secret])
                .unwrap();
            db.execute("INSERT INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot,change_blob,proof) VALUES(?1,?2,NULL,0,0,X'',?3,?4,?5)",params![gid,branch,snapshot,b"historical group change".as_slice(),b"historical group proof".as_slice()]).unwrap();
            db.execute("INSERT INTO mls_join_history VALUES(?1,?2,?3)",params![gid,[114u8;32],b"historical joined group metadata".as_slice()]).unwrap();
            db.execute("INSERT INTO mls_group_received(group_id,branch,sender,dispatch_id,accepted_at_ms,payload) VALUES(?1,?2,?3,?4,0,?5)",params![gid,branch,[114u8;32],[115u8;16],b"old-journal-message".as_slice()]).unwrap();
        }
        crate::privacy::lock(&conv, "transport-secret").unwrap();
        let provider = PromtuzMlsProvider::shared();
        {
            let db = mls.lock();
            let raw: Vec<u8> = db
                .query_row("SELECT value FROM mls_storage WHERE group_id=?1", [&key], |r| r.get(0))
                .unwrap();
            assert!(sealed(&raw));
            assert_ne!(raw, secret);
            assert!(open_storage(&db, &key, &raw).is_err());
            let bytes: Vec<u8> = db
                .query_row("SELECT snapshot FROM mls_branches WHERE group_id=?1", [gid], |r| {
                    r.get(0)
                })
                .unwrap();
            let rows: Vec<(i64, Vec<u8>, Vec<u8>)> =
                postcard::from_bytes(&lz4_flex::decompress_size_prepended(&bytes).unwrap())
                    .unwrap();
            assert!(sealed(&rows[0].2));
            for (table,column) in [("mls_branches","change_blob"),("mls_branches","proof"),("mls_join_history","history")] {
                let value:Vec<u8>=db.query_row(&format!("SELECT {column} FROM {table} WHERE group_id=?1"),[gid],|r|r.get(0)).unwrap();
                assert!(sealed(&value), "{table}.{column} leaks historical group metadata");
                assert!(open_payload(&db,&gid,&value).is_err());
            }
            let raw: Vec<u8> = db
                .query_row("SELECT payload FROM mls_group_received WHERE group_id=?1", [gid], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(sealed(&raw));
        }
        let envelope = common::proto::mls_wire::MlsEnvelopeP::Application(
            common::proto::mls_wire::MlsApplicationEnvelopeP {
                version: common::proto::mls_wire::MLS_ENVELOPE_VERSION,
                group_id: gid.into(),
                epoch: 0,
                mls_message: b"encrypted-transport-bytes".to_vec().into(),
                sender_sig: [0u8; 64].into(),
            },
        );
        let bytes = envelope.ser().unwrap();
        assert!(maybe_buffer(&provider, &envelope, &[116; 32], &bytes, 9, &[117; 16]).unwrap());
        assert!(maybe_buffer(&provider, &envelope, &[116; 32], &bytes, 9, &[117; 16]).unwrap());
        assert_eq!(
            mls.lock()
                .query_row("SELECT count(*) FROM mls_private_inbox", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(crate::privacy::unlock("transport-secret").unwrap(), Some(conv));
        {
            let db = mls.lock();
            let raw: Vec<u8> = db
                .query_row("SELECT value FROM mls_storage WHERE group_id=?1", [&key], |r| r.get(0))
                .unwrap();
            assert_eq!(open_storage(&db, &key, &raw).unwrap(), secret);
            let raw: Vec<u8> = db
                .query_row("SELECT payload FROM mls_group_received WHERE group_id=?1", [gid], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(open_payload(&db, &gid, &raw).unwrap(), b"old-journal-message");
        }
        assert!(!maybe_buffer(&provider, &envelope, &[116; 32], &bytes, 9, &[118; 16]).unwrap());
        crate::api::privacy::lock_private_conversations();
        let db = mls.lock();
        assert!(
            open_storage(
                &db,
                &key,
                &db.query_row::<Vec<u8>, _, _>(
                    "SELECT value FROM mls_storage WHERE group_id=?1",
                    [&key],
                    |r| r.get(0)
                )
                .unwrap()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::data::{contact::Contact, conversation::Conversation, message::Message};
    use crate::messaging::{
        receive::process_inbound_envelope,
        send::seal_application_message,
        session::{MlsContext, lazy_create_group, leaf_signer_for_group},
    };
    use crate::test_support::{
        ScopedCore,
        data::identity,
        net::{Device, FakeDhtClient},
    };
    use common::proto::{
        mls_wire::{AppPayload, Body},
        pack::Packer,
    };

    #[tokio::test]
    async fn a_hidden_chat_buffers_real_traffic_and_a_rebind_without_naming_the_sender() {
        let scope = ScopedCore::new();
        let signer = identity(&scope.core.db.identity().lock(), 131);
        let ipk = signer.verifying_key().to_bytes();
        let bob = Device::new(132);
        let dht = FakeDhtClient::default();
        Contact::save(bob.ipk, "Bob".into()).unwrap();
        let conv = Conversation::for_peer(&bob.ipk).unwrap();
        let provider = super::super::PromtuzMlsProvider::shared();
        let stash = super::super::KeyPackageStash::new(scope.core.db.mls());
        let buffer = super::super::EpochCatchupBuffer::new(scope.core.db.mls());
        let ctx = MlsContext { provider: &provider, stash: &stash, buffer: &buffer, dht: &dht };
        bob.publish_keypackage(&dht).await;
        let group = lazy_create_group(&ctx, &ipk, &signer, &bob.ipk).await.unwrap();
        let gid = group.group_id();
        drop(group);
        Conversation::bind_group(&conv, &gid).unwrap();
        let welcome = dht.welcomes_pending.lock().pop().unwrap();
        let mut remote = crate::mls::process_welcome(&bob.provider, &welcome.envelope).unwrap();
        crate::api::privacy::hide_conversation(conv.to_vec(), Some("hidden-transport".into())).unwrap();
        crate::api::privacy::lock_private_conversations();
        let leaf = leaf_signer_for_group(&bob.provider, &remote, &bob.ipk).unwrap();
        let payload = AppPayload::Post { reply_to: None, body: Body::Text("hidden secret".into()) }.ser().unwrap();
        let bytes = seal_application_message(&bob.provider, &mut remote, &leaf, &payload)
            .unwrap()
            .address_to(&ipk, &bob.signer)
            .unwrap();
        process_inbound_envelope(&ctx, bob.ipk, &bytes, 1000, [133; 16]).await.unwrap();
        let named = |scope: &ScopedCore| -> u32 {
            scope.core.db.mls().lock().query_row("SELECT count(*) FROM mls_private_inbox WHERE sender=?1", [bob.ipk], |r| r.get(0)).unwrap()
        };
        assert_eq!(named(&scope), 0);
        assert!(!Contact::exists(&bob.ipk));

        // Bob starts over from his side: the Welcome is filed into the vault, not a request.
        use crate::quic::dht_client::DhtClient;
        dht.publish_keypackages(&[stash.generate_one(&provider, &signer).unwrap()]).await.unwrap();
        let next = lazy_create_group(&bob.ctx(&dht), &bob.ipk, &bob.signer, &ipk).await.unwrap();
        let next_gid = next.group_id();
        drop(next);
        let welcome = dht.welcomes_pending.lock().pop().unwrap();
        let bytes = common::proto::mls_wire::MlsEnvelopeP::Welcome(welcome.envelope).ser().unwrap();
        process_inbound_envelope(&ctx, bob.ipk, &bytes, 2000, [134; 16]).await.unwrap();
        assert_eq!(named(&scope), 0);
        assert!(!Contact::exists(&bob.ipk));
        assert!(crate::api::messaging::list_conversations().iter().all(|c| c.id != conv));

        crate::privacy::unlock("hidden-transport").unwrap();
        drain_with(&ctx, conv).await.unwrap();
        let rows = Message::get_messages(&conv, 100, "");
        assert_eq!(rows[0].content, "hidden secret");
        assert_eq!(rows[0].sender_ipk, Some(bob.ipk.to_vec()));
        assert_eq!(Conversation::group_of(&conv), Some(next_gid));
        assert!(Contact::exists(&bob.ipk));
        crate::api::privacy::lock_private_conversations();
        assert!(!Contact::exists(&bob.ipk));
    }

    #[tokio::test]
    async fn private_real_transport_buffers_then_decrypts_and_relocks() {
        let scope = ScopedCore::new();
        let signer = identity(&scope.core.db.identity().lock(), 121);
        let ipk = signer.verifying_key().to_bytes();
        let bob = Device::new(122);
        let dht = FakeDhtClient::default();
        Contact::save(bob.ipk, "Bob".into()).unwrap();
        let conv = Conversation::for_peer(&bob.ipk).unwrap();
        let provider = super::super::PromtuzMlsProvider::shared();
        let stash = super::super::KeyPackageStash::new(scope.core.db.mls());
        let buffer = super::super::EpochCatchupBuffer::new(scope.core.db.mls());
        let ctx = MlsContext { provider: &provider, stash: &stash, buffer: &buffer, dht: &dht };
        bob.publish_keypackage(&dht).await;
        let group = lazy_create_group(&ctx, &ipk, &signer, &bob.ipk).await.unwrap();
        let gid = group.group_id();
        drop(group);
        Conversation::bind_group(&conv, &gid).unwrap();
        let welcome = dht.welcomes_pending.lock().pop().unwrap();
        let mut remote = crate::mls::process_welcome(&bob.provider, &welcome.envelope).unwrap();
        crate::api::privacy::lock_conversation(conv.to_vec(), "transport-code".into()).unwrap();
        let leaf = leaf_signer_for_group(&bob.provider, &remote, &bob.ipk).unwrap();
        let payload =
            AppPayload::Post { reply_to: None, body: Body::Text("buffered real secret".into()) }
                .ser()
                .unwrap();
        let bytes = seal_application_message(&bob.provider, &mut remote, &leaf, &payload)
            .unwrap()
            .address_to(&ipk, &bob.signer)
            .unwrap();
        process_inbound_envelope(&ctx, bob.ipk, &bytes, 1000, [123; 16]).await.unwrap();
        assert!(Message::get_messages(&conv, 100, "").is_empty());
        assert_eq!(
            scope
                .core
                .db
                .mls()
                .lock()
                .query_row("SELECT count(*) FROM mls_private_inbox", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            1
        );
        crate::privacy::unlock("transport-code").unwrap();
        drain_with(&ctx, conv).await.unwrap();
        assert_eq!(Message::get_messages(&conv, 100, "")[0].content, "buffered real secret");
        assert_eq!(
            scope
                .core
                .db
                .mls()
                .lock()
                .query_row("SELECT count(*) FROM mls_private_inbox", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
        let raw: String = scope
            .core
            .db
            .messages()
            .lock()
            .query_row("SELECT content FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.contains("buffered real secret"));
        crate::api::privacy::lock_private_conversations();
        assert!(crate::mls::MlsGroupHandle::load(&provider, &gid).is_err());
        assert!(crate::api::messaging::get_messages(conv.to_vec(), 100, "".into()).is_err());
        // A restored/unbound hidden conversation still protects its very first key write.
        crate::privacy::unlock("transport-code").unwrap();
        let replacement = [124u8; 32];
        prepare_outgoing_direct(&provider, &bob.ipk, &replacement).unwrap();
        let (leaf, credential) = crate::messaging::session::build_self_credential(&signer).unwrap();
        leaf.store(provider.storage()).unwrap();
        let g =
            crate::mls::MlsGroupHandle::create(&provider, &leaf, credential, &replacement, None)
                .unwrap();
        drop(g);
        let raw:Vec<Vec<u8>>=crate::db::all(&scope.core.db.mls().lock(),"SELECT value FROM mls_storage WHERE group_id=(SELECT storage_key FROM mls_private_conversations WHERE group_id=?1)",[replacement],|r|r.get(0)).unwrap();
        assert!(!raw.is_empty());
        assert!(raw.iter().all(|v| sealed(v)));
        // A fresh Welcome for a hidden contact is code-wrapped before its ordinary,
        // unconsumed KeyPackage can be used. Unlock drains Welcome before later traffic.
        use crate::quic::dht_client::DhtClient;
        dht.publish_keypackages(&[stash.generate_one(&provider,&signer).unwrap()]).await.unwrap();
        let next = lazy_create_group(&bob.ctx(&dht),&bob.ipk,&bob.signer,&ipk).await.unwrap();
        let next_gid=next.group_id();drop(next);
        let welcome=dht.welcomes_pending.lock().pop().unwrap();
        let bytes=common::proto::mls_wire::MlsEnvelopeP::Welcome(welcome.envelope).ser().unwrap();
        crate::api::privacy::lock_private_conversations();
        process_inbound_envelope(&ctx,bob.ipk,&bytes,2000,[125;16]).await.unwrap();
        let raw:Vec<u8>=scope.core.db.mls().lock().query_row("SELECT envelope FROM mls_private_inbox WHERE group_id=?1",[next_gid],|r|r.get(0)).unwrap();
        assert!(sealed(&raw));assert_ne!(raw,bytes);
        assert!(crate::mls::MlsGroupHandle::load(&provider,&next_gid).unwrap().is_none());
        crate::privacy::unlock("transport-code").unwrap();
        drain_with(&ctx,conv).await.unwrap();
        assert_eq!(Conversation::group_of(&conv),Some(next_gid));
        assert!(crate::mls::MlsGroupHandle::load(&provider,&next_gid).unwrap().is_some());
        crate::api::privacy::lock_private_conversations();
        assert!(crate::mls::MlsGroupHandle::load(&provider,&next_gid).is_err());

    }

}

#[cfg(test)]
mod group_transport_tests {
    use super::*;
    use crate::data::{conversation::Conversation, message::Message};
    use crate::messaging::{receive::process_inbound_envelope, send::seal_application_message, session::MlsContext};
    use crate::mls::{MlsGroupHandle, GroupMeta, branch_proof as proof, recovery as journal};
    use crate::test_support::{ScopedCore, mls::{Party, kp_ref}, net::FakeDhtClient};
    use common::proto::{mls_wire::{AppPayload, Body, GroupChange, GroupMessage}, pack::Packer};
    use openmls::prelude::{KeyPackage, MlsMessageOut, tls_codec::Serialize as _};

    // Real signed branch transitions, including a fresh member-owned KeyPackage on reset.
    fn commit(by:&Party,gid:[u8;32],change:Option<GroupChange>,adds:Vec<KeyPackage>)->(MlsMessageOut,Vec<u8>) {
        let mut tx=journal::Transaction::open(&by.provider,gid,None).unwrap().unwrap();
        let before=tx.group.group_meta().unwrap().effective();
        let signed=change.map(|change|by.sign(&tx.group,change));
        let (message,welcome)=if let Some(signed)=&signed {
            let mut meta=tx.group.group_meta().unwrap();
            meta.state=Some(tx.group.state_after_change(&by.ipk,signed).unwrap());
            let removes=if let GroupChange::MemberRequest(request)=&signed.change {
                vec![tx.group.member_index_by_ipk(&request.who.0).unwrap()]
            } else {vec![]};
            tx.group.commit_meta(&tx.provider,&by.leaf,&meta,adds,removes).unwrap()
        } else {
            let (message,welcome)=tx.group.add_members(&tx.provider,&by.leaf,&adds).unwrap();
            (message,Some(welcome))
        };
        let bytes=message.tls_serialize_detached().unwrap();
        tx.group.merge_pending_commit(&tx.provider).unwrap();
        let branch=proof::sign(&tx.group,Some(tx.parent),before.role(&by.ipk),&bytes,&by.identity).unwrap();
        let history=journal::next_history(&by.provider,&gid,tx.parent,&branch).unwrap();
        let encrypted=proof::seal(&tx.provider,&tx.group,proof::INVITATION_LABEL,&postcard::to_allocvec(&history).unwrap()).unwrap();
        tx.publish(Some(journal::Candidate{rank:branch.rank,message:bytes,change:signed,proof:Some(branch)}),&[],None,None).unwrap();
        (welcome.unwrap(),encrypted)
    }
    fn post(by:&Party,gid:[u8;32],to:&[u8;32],id:[u8;16],text:&str)->Vec<u8> {
        let mut tx=journal::Transaction::open(&by.provider,gid,None).unwrap().unwrap();
        let payload=AppPayload::Post{reply_to:None,body:Body::Text(text.into())}.ser().unwrap();
        let payload=postcard::to_allocvec(&GroupMessage{id:id.into(),payload:payload.into()}).unwrap();
        let mut sealed=seal_application_message(&tx.provider,&mut tx.group,&by.leaf,&payload).unwrap();
        sealed.branch=Some(tx.parent);
        let bytes=sealed.address_to(to,&by.identity).unwrap();
        tx.publish(None,&[],None,None).unwrap();
        bytes
    }

    #[tokio::test]
    async fn signed_group_lock_buffers_real_traffic_and_reset_recovers_only_fresh_membership() {
        let scope=ScopedCore::new();
        let signer=crate::test_support::data::identity(&scope.core.db.identity().lock(),141);
        let ipk=signer.verifying_key().to_bytes();
        let (leaf,_)=crate::messaging::session::build_self_credential(&signer).unwrap();
        let provider=super::super::PromtuzMlsProvider::shared();
        leaf.store(provider.storage()).unwrap();
        let local=Party{ipk,identity:signer,leaf,db:scope.core.db.mls(),provider};
        let remote=Party::new(142);
        crate::data::contact::Contact::save(remote.ipk,"Member".into()).unwrap();
        let gid=[143;32];
        let meta=GroupMeta::founded("Private signed group".into(),ipk);
        let group=MlsGroupHandle::create(&local.provider,&local.leaf,local.credential(),&gid,Some(&meta)).unwrap();
        journal::ensure_root(&local.provider,&group,&local.identity).unwrap();
        drop(group);
        let kp=remote.kp();
        let (welcome,history)=commit(&local,gid,None,vec![kp.clone()]);
        let envelope=local.envelope(&remote,gid,welcome,&kp);
        journal::accept_welcome(&remote.provider,&envelope,&history,None,None).unwrap();
        let group=local.group(&gid);
        let anchor=journal::history(&local.provider,&gid,group.branch_id()).unwrap()[0].branch.0;
        let conv=Conversation::create_group("Private signed group",&[remote.ipk]).unwrap();
        Conversation::bind_group(&conv,&gid).unwrap();
        Conversation::sync_group(&conv,&group.roster(),group.group_meta().as_ref()).unwrap();
        crate::groups::member_requests::anchor(&gid,anchor).unwrap();
        drop(group);
        let stash=super::super::KeyPackageStash::new(scope.core.db.mls());
        let buffer=super::super::EpochCatchupBuffer::new(scope.core.db.mls());
        let dht=FakeDhtClient::default();
        let ctx=MlsContext{provider:&local.provider,stash:&stash,buffer:&buffer,dht:&dht};
        Message::save_incoming(conv,remote.ipk,&[144;16],"before locking",1,None).unwrap();
        crate::api::privacy::lock_conversation(conv.to_vec(),"signed-group-code".into()).unwrap();
        assert!(MlsGroupHandle::load(&local.provider,&gid).is_err());
        assert!(journal::history(&local.provider,&gid,remote.group(&gid).branch_id()).is_err());
        let first=post(&remote,gid,&ipk,[145;16],"arrived while locked");
        // An authenticated transport sender can still provide an invalid inner MLS envelope.
        // Reject that entry on unlock without letting it block later valid group traffic.
        use common::proto::pack::Unpacker;
        let mut invalid=common::proto::mls_wire::MlsEnvelopeP::deser(&first).unwrap();
        if let common::proto::mls_wire::MlsEnvelopeP::GroupApplication{message,..}=&mut invalid {
            message.sender_sig.0[0]^=1;
        } else {unreachable!()}
        process_inbound_envelope(&ctx,remote.ipk,&invalid.ser().unwrap(),1999,[148;16]).await.unwrap();
        process_inbound_envelope(&ctx,remote.ipk,&first,2000,[145;16]).await.unwrap();
        assert!(crate::api::messaging::get_messages(conv.to_vec(),100,"".into()).is_err());
        assert_eq!(scope.core.db.messages().lock().query_row("SELECT count(*) FROM messages WHERE conversation_id=?1",[conv],|r|r.get::<_,u32>(0)).unwrap(),1);
        assert!(crate::privacy::unlock("incorrect-code").unwrap().is_none());
        crate::privacy::unlock("signed-group-code").unwrap();
        let interrupted=core().private_generation.load(std::sync::atomic::Ordering::SeqCst);
        crate::api::privacy::lock_private_conversations();
        crate::privacy::unlock("signed-group-code").unwrap();
        drain_generation(&ctx,conv,interrupted).await.unwrap();
        assert_eq!(scope.core.db.mls().lock().query_row("SELECT count(*) FROM mls_private_inbox WHERE group_id=?1",[gid],|r|r.get::<_,u32>(0)).unwrap(),2,"a drain from an interrupted unlock must leave its inbox for the current session");
        scope.core.db.mls().lock().execute_batch("CREATE TRIGGER refuse_private_ratchet BEFORE UPDATE ON mls_storage WHEN length(NEW.group_id)>0 BEGIN SELECT RAISE(ABORT,'injected ratchet storage failure'); END").unwrap();
        let failure=drain_with(&ctx,conv).await.unwrap_err();
        assert!(crate::utils::is_storage_error(&failure));
        assert_eq!(scope.core.db.mls().lock().query_row("SELECT count(*) FROM mls_private_inbox WHERE group_id=?1",[gid],|r|r.get::<_,u32>(0)).unwrap(),1);
        scope.core.db.mls().lock().execute_batch("DROP TRIGGER refuse_private_ratchet").unwrap();
        scope.core.db.messages().lock().execute_batch("CREATE TRIGGER refuse_private_delivery BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT,'injected storage failure'); END").unwrap();
        let failure=drain_with(&ctx,conv).await.unwrap_err();
        assert!(crate::utils::is_storage_error(&failure));
        assert_eq!(scope.core.db.mls().lock().query_row("SELECT count(*) FROM mls_private_inbox WHERE group_id=?1",[gid],|r|r.get::<_,u32>(0)).unwrap(),1,"rejected envelopes are removed but failed durable writes remain retryable");
        scope.core.db.messages().lock().execute_batch("DROP TRIGGER refuse_private_delivery").unwrap();
        drain_with(&ctx,conv).await.unwrap();
        assert_eq!(scope.core.db.mls().lock().query_row("SELECT count(*) FROM mls_private_inbox WHERE group_id=?1",[gid],|r|r.get::<_,u32>(0)).unwrap(),0);
        assert_eq!(Message::get_messages(&conv,100,"").iter().map(|m|m.content.as_str()).collect::<Vec<_>>(),["before locking","arrived while locked"]);
        assert!(Message::pending_notification_ids(&conv).unwrap().is_empty());
        let group=local.group(&gid);
        assert_eq!(group.roster().len(),2);
        assert_eq!(journal::history(&local.provider,&gid,group.branch_id()).unwrap()[0].branch.0,anchor);
        drop(group);
        crate::api::privacy::lock_private_conversations();
        crate::api::privacy::reset_private_conversation(conv.to_vec()).unwrap();
        assert_eq!(Conversation::group_of(&conv),Some(gid));
        assert!(MlsGroupHandle::load(&local.provider,&gid).unwrap().is_none());
        assert!(Message::get_messages(&conv,100,"").is_empty());
        assert!(crate::privacy::unlock("signed-group-code").unwrap().is_none());
        let request=crate::groups::member_requests::refresh_request(&gid).unwrap();
        // A new member-owned key is required. No account key, former branch snapshot,
        // buffered dispatch or old Welcome can restore the discarded historical ratchets.
        let kp=local.kp();
        let (welcome,history)=commit(&remote,gid,Some(GroupChange::MemberRequest(request)),vec![kp.clone()]);
        let envelope=crate::mls::make_welcome_envelope(welcome,gid,remote.ipk,ipk,kp_ref(&kp),&remote.identity).unwrap();
        let recovery=crate::groups::recovery::welcome_envelope(envelope,&history,&remote.identity).unwrap().ser().unwrap();
        scope.core.db.mls().lock().execute_batch("CREATE TRIGGER refuse_recovery_welcome BEFORE INSERT ON mls_storage WHEN length(NEW.group_id)>0 BEGIN SELECT RAISE(ABORT,'injected Welcome storage failure'); END").unwrap();
        let failure=process_inbound_envelope(&ctx,remote.ipk,&recovery,3000,[146;16]).await.unwrap_err();
        assert!(crate::utils::is_storage_error(&failure));
        assert!(MlsGroupHandle::load(&local.provider,&gid).unwrap().is_none());
        assert!(crate::groups::member_requests::refresh_request(&gid).is_some());
        scope.core.db.mls().lock().execute_batch("DROP TRIGGER refuse_recovery_welcome").unwrap();
        process_inbound_envelope(&ctx,remote.ipk,&recovery,3000,[146;16]).await.unwrap();
        assert!(MlsGroupHandle::load(&local.provider,&gid).unwrap().is_some());
        assert!(Message::get_messages(&conv,100,"").is_empty());
        assert!(crate::groups::member_requests::refresh_request(&gid).is_none());
        let next=post(&remote,gid,&ipk,[147;16],"only new history");
        process_inbound_envelope(&ctx,remote.ipk,&next,4000,[147;16]).await.unwrap();
        assert_eq!(Message::get_messages(&conv,100,"").iter().filter(|m|m.system==0).map(|m|m.content.as_str()).collect::<Vec<_>>(),["only new history"]);
    }
}
