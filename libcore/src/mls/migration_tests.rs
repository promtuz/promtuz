//! Unanimous migration of groups from before bound credentials.

use common::proto::client_rel::CRelayPacket;
use common::proto::mls_wire::GroupMigrationApproval;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::group_migration_welcome_signing_input;
use common::proto::pack::Unpacker;
use ed25519_dalek::Signer as _;
use openmls::prelude::KeyPackage;
use openmls::prelude::ProcessedMessageContent;

use super::GroupMeta;
use super::MlsGroupHandle;
use super::PromtuzMlsProvider;
use super::migration;
use super::migration::Source;
use crate::test_support::mls::*;

/// A fresh bound KeyPackage for the new session, as the founder's request for it returns.
fn fresh(p: &Party) -> ([u8; 32], KeyPackage, [u8; 32]) {
    let kp = p.kp();
    let reference = kp_ref(&kp);
    (p.ipk, kp, reference)
}

/// A legacy group of three whose members joined before credentials were bound, and every
/// member's approval of its current state.
fn fixture(gid: [u8; 32]) -> (Party, Party, Party, Vec<GroupMigrationApproval>) {
    let (a, b, c) = (Party::new(71), Party::new(72), Party::new(73));
    let meta = GroupMeta { title: "Old group".into(), founder: a.ipk, state: None };
    let mut ga =
        MlsGroupHandle::create(&a.provider, &a.leaf, a.legacy_credential(), &gid, Some(&meta))
            .unwrap();
    let kps = [&b, &c].map(|p| p.key_package(p.legacy_credential()));
    let (_, welcome) = ga.add_members(&a.provider, &a.leaf, &kps).unwrap();
    ga.merge_pending_commit(&a.provider).unwrap();
    b.join(&welcome);
    c.join(&welcome);
    let source = Source::read(&ga).unwrap().unwrap();
    let mut approvals =
        [&a, &b, &c].map(|p| source.approve(&p.group(&gid), &p.identity).unwrap()).to_vec();
    approvals.sort_by_key(|a| a.who.0);
    (a, b, c, approvals)
}

/// The invitation `founder` queued for `who` in its outbox jobs.
fn invitation(founder: &Party, target: [u8; 32], who: [u8; 32]) -> MlsEnvelopeP {
    let sql = "SELECT frame FROM mls_dispatch_jobs WHERE group_id = ?1 AND recipient = ?2";
    let frame: Vec<u8> =
        founder.db.lock().query_row(sql, rusqlite::params![target, who], |r| r.get(0)).unwrap();
    let CRelayPacket::Dispatch(dispatch) = CRelayPacket::deser(&frame[4..]).unwrap() else {
        panic!("not a dispatch")
    };
    MlsEnvelopeP::deser(&dispatch.payload).unwrap()
}

fn accept(r: &Party, from: [u8; 32], envelope: MlsEnvelopeP) -> anyhow::Result<[u8; 32]> {
    let MlsEnvelopeP::GroupMigrationWelcome {
        group,
        branch,
        approvals,
        welcome,
        history,
        signature,
    } = envelope
    else {
        panic!("not a migration")
    };
    let conversation = [r.ipk[0]; 16];
    migration::accept(
        &r.provider,
        group.0,
        branch.0,
        conversation,
        r.ipk,
        from,
        &approvals,
        &welcome,
        &history.0,
        &signature.0,
    )
}

/// Only the founder migrates, with every member's approval; a failure on disk at either end keeps
/// the old session and burns no keys, and redelivery is idempotent.
#[test]
fn migration_requires_every_identity_and_survives_failed_publication_and_redelivery() {
    let gid = [0x71; 32];
    let (a, b, c, approvals) = fixture(gid);
    let packages = vec![fresh(&b), fresh(&c)];
    let source = Source::read(&a.group(&gid)).unwrap().unwrap();
    let target = source.target();
    let create = |signer, approvals: &[GroupMigrationApproval]| {
        migration::create(&a.provider, gid, [71; 16], signer, approvals, &packages)
    };
    assert!(create(&a.identity, &approvals[..2]).is_err(), "every member approves");
    let mut forged = approvals.clone();
    forged[1].signature = forged[0].signature;
    assert!(create(&a.identity, &forged).is_err(), "each approval is its member's");
    assert!(create(&b.identity, &approvals).is_err(), "only the founder migrates");

    let failed = with_failing_trigger(&a.db, "INSERT ON mls_group_migrations", || {
        create(&a.identity, &approvals)
    });
    assert!(failed.is_err());
    assert_eq!(a.group(&gid).branch_id(), source.branch);
    assert!(MlsGroupHandle::load(&a.provider, &target).unwrap().is_none());
    let jobs = a.db.lock().query_row("SELECT COUNT(*) FROM mls_dispatch_jobs", [], |r| r.get(0));
    assert_eq!(jobs, Ok(0), "no invitation is queued");

    assert_eq!(create(&a.identity, &approvals).unwrap(), target);
    assert!(MlsGroupHandle::load(&a.provider, &gid).unwrap().is_none(), "the old session retires");
    let restarted = PromtuzMlsProvider::new(a.db.clone());
    assert_eq!(migration::completed(&restarted, &gid).unwrap(), Some((target, [71; 16])));

    let b_invite = invitation(&a, target, b.ipk);
    let mut tampered = b_invite.clone();
    if let MlsEnvelopeP::GroupMigrationWelcome {
        group,
        branch,
        approvals,
        welcome,
        history,
        signature,
    } = &mut tampered
    {
        history.0[0] ^= 1;
        let input = group_migration_welcome_signing_input(
            &group.0, &branch.0, approvals, welcome, &history.0,
        );
        *signature = a.identity.sign(&input).to_bytes().into();
    }
    assert!(accept(&b, a.ipk, tampered).is_err());
    assert_eq!(b.group(&gid).branch_id(), source.branch);
    let late_failure = "INSERT ON mls_branches";
    assert!(
        with_failing_trigger(&b.db, late_failure, || accept(&b, a.ipk, b_invite.clone())).is_err()
    );
    assert_eq!(b.group(&gid).branch_id(), source.branch);
    assert_eq!(accept(&b, a.ipk, b_invite.clone()).unwrap(), target, "no keys were burned");
    assert_eq!(accept(&b, a.ipk, b_invite).unwrap(), target, "redelivery is idempotent");
    accept(&c, a.ipk, invitation(&a, target, c.ipk)).unwrap();

    let mut ga = a.group(&target);
    for r in [&a, &b, &c] {
        let group = r.group(&target);
        assert_eq!(group.branch_id(), ga.branch_id());
        assert!(group.group_meta().unwrap().state.is_some());
        assert_eq!(group.roster().len(), 3);
    }
    let signer = crate::messaging::session::leaf_signer_for_group(&a.provider, &ga, &a.ipk).unwrap();
    let msg = ga.create_application_message(&a.provider, &signer, b"same conversation").unwrap();
    for r in [&b, &c] {
        let decoded = r.group(&target).process_incoming(&r.provider, wire(&msg)).unwrap();
        assert_eq!(decoded.sender, a.ipk);
        let ProcessedMessageContent::ApplicationMessage(body) = decoded.content else { panic!() };
        assert_eq!(body.into_bytes(), b"same conversation");
    }
}

/// A member whose group moved on refuses an invitation for the old state and keeps its session.
#[test]
fn migration_rejects_a_different_epoch_and_preserves_the_old_session() {
    let gid = [0x72; 32];
    let (a, b, c, approvals) = fixture(gid);
    let packages = [fresh(&b), fresh(&c)];
    let target =
        migration::create(&a.provider, gid, [71; 16], &a.identity, &approvals, &packages).unwrap();
    let mut gb = b.group(&gid);
    b.update(&mut gb);
    gb.merge_pending_commit(&b.provider).unwrap();
    let advanced = gb.branch_id();
    assert!(accept(&b, a.ipk, invitation(&a, target, b.ipk)).is_err());
    assert_eq!(b.group(&gid).branch_id(), advanced);
    assert!(MlsGroupHandle::load(&b.provider, &target).unwrap().is_none());
}

/// Installs that still hold bare legacy credentials keep messaging in a pair, which never enters
/// group migration.
#[test]
fn legacy_direct_credentials_still_send_without_group_migration() {
    let (a, b) = (Party::new(74), Party::new(75));
    let gid = [0x74; 32];
    let mut group =
        MlsGroupHandle::create(&a.provider, &a.leaf, a.legacy_credential(), &gid, None).unwrap();
    let legacy = b.key_package(b.legacy_credential());
    let (_, welcome) = group.add_members(&a.provider, &a.leaf, &[legacy]).unwrap();
    group.merge_pending_commit(&a.provider).unwrap();
    let mut other = b.join(&welcome);
    assert!(Source::read(&group).unwrap().is_none());
    let signer = crate::messaging::session::leaf_signer_for_group(&a.provider, &group, &a.ipk).unwrap();
    let msg = group.create_application_message(&a.provider, &signer, b"legacy pair").unwrap();
    assert_eq!(other.process_incoming(&b.provider, wire(&msg)).unwrap().sender, a.ipk);
}

/// Moving the chat to the new session keeps everything it shows, survives a failed switch, and
/// cannot bring back a chat deleted since.
#[tokio::test]
async fn migration_projection_preserves_content_and_recovers_a_failed_database_switch() {
    use rusqlite::params;

    use crate::data::conversation::Conversation;
    use crate::groups::migration::destination;
    use crate::groups::migration::finish;
    use crate::groups::migration::retired;

    let scope = crate::test_support::ScopedCore::new();
    let db = scope.core.db.messages();
    let gid = [0x73; 32];
    let (a, b, c, approvals) = fixture(gid);
    let (conversation, did, message) = ([71; 16], [72u8; 16], crate::test_support::data::ulid(1));
    {
        let conn = db.lock();
        conn.execute(
            "INSERT INTO conversations(id,kind,created_by,mls_group_id,title,pinned,muted) \
             VALUES(?1,1,?2,?3,'Saved name',1,1)",
            params![conversation, a.ipk, gid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(id,conversation_id,content,outgoing,timestamp,status,dispatch_id) \
             VALUES(?1,?2,'Keep this attachment',0,42,1,?3)",
            params![message, conversation, did],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_media(conversation_id,dispatch_id,kind,mime,blob,file_id) \
             VALUES(?1,?2,1,'image/avif',?3,?4)",
            params![conversation, did, vec![7u8; 128], vec![9u8; 32]],
        )
        .unwrap();
    }
    let packages = [fresh(&b), fresh(&c)];
    let target =
        migration::create(&a.provider, gid, conversation, &a.identity, &approvals, &packages)
            .unwrap();
    let switch = || finish(&a.provider, gid);
    assert!(with_failing_trigger(db, "UPDATE ON conversations", switch).is_err());
    assert_eq!(Conversation::group_of(&conversation), Some(gid));
    switch().unwrap();
    switch().unwrap();
    assert_eq!(Conversation::group_of(&conversation), Some(target));
    assert_eq!(Conversation::active_members(&conversation).len(), 3);
    assert!(Conversation::has_signed_rules(&conversation));
    assert!(retired(&a.provider, &gid).unwrap());
    let wiped = PromtuzMlsProvider::new(crate::db::Stores::in_memory(String::new()).mls());
    assert!(retired(&wiped, &gid).unwrap(), "the marker retires the session without MLS storage");

    let conn = db.lock();
    let sql = "SELECT title, pinned, muted FROM conversations WHERE id = ?1";
    let kept = conn.query_row(sql, [conversation], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)));
    assert_eq!(kept, Ok(("Saved name".to_string(), true, true)));
    let sql = "SELECT content FROM messages WHERE id = ?1";
    let content = conn.query_row(sql, [&message], |r| r.get::<_, String>(0));
    assert_eq!(content.unwrap(), "Keep this attachment");
    let sql = "SELECT blob, file_id FROM message_media WHERE conversation_id = ?1";
    let media = conn.query_row(sql, [conversation], |r| Ok((r.get(0)?, r.get(1)?)));
    assert_eq!(media, Ok((vec![7u8; 128], vec![9u8; 32])));
    conn.execute("DELETE FROM conversations WHERE id = ?1", [conversation]).unwrap();
    drop(conn);
    assert!(destination(&a.provider, &target).is_err());
    assert!(Conversation::get(&conversation).is_none(), "a late migration cannot recreate it");
}
