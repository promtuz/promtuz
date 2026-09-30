mod legacy_migration {
    use common::proto::mls_wire::GroupMigrationApproval;
    use common::proto::mls_wire::MlsEnvelopeP;
    use common::proto::pack::Unpacker;
    use ed25519_dalek::Signer;

    use super::*;
    use crate::mls::migration::Source;
    use crate::mls::migration::{
        self,
    };

    struct Replica {
        provider: PromtuzMlsProvider,
        party:    Party,
    }
    impl Replica {
        fn new(seed: u8) -> Self {
            let provider = build_provider();
            let party = Party::new(&provider, seed);
            Self { provider, party }
        }
        fn credential(&self) -> CredentialWithKey {
            CredentialWithKey {
                credential:    BasicCredential::new(self.party.ipk.to_vec()).into(),
                signature_key: self.party.sig_kp.public().into(),
            }
        }
        fn legacy_kp(&self) -> KeyPackage {
            KeyPackage::builder()
                .leaf_node_capabilities(Capabilities::new(
                    None,
                    Some(&[PROMTUZ_CIPHERSUITE]),
                    Some(&[GROUP_META_EXTENSION]),
                    None,
                    None,
                ))
                .build(PROMTUZ_CIPHERSUITE, &self.provider, &self.party.sig_kp, self.credential())
                .unwrap()
                .key_package()
                .clone()
        }
        fn group(&self, gid: &[u8; 32]) -> MlsGroupHandle {
            MlsGroupHandle::load(&self.provider, gid).unwrap().unwrap()
        }
        fn fresh(&self) -> ([u8; 32], KeyPackage, [u8; 32]) {
            let kp = make_kp(&self.provider, &self.party);
            let reference =
                kp.hash_ref(self.provider.crypto()).unwrap().as_slice().try_into().unwrap();
            (self.party.ipk, kp, reference)
        }
    }

    fn fixture() -> ([u8; 32], Replica, Replica, Replica, Vec<GroupMigrationApproval>) {
        let (a, b, c) = (Replica::new(71), Replica::new(72), Replica::new(73));
        let gid = rand::random();
        let mut ga = MlsGroupHandle::create(
            &a.provider,
            &a.party.sig_kp,
            a.credential(),
            &gid,
            Some(&GroupMeta { title: "Old group".into(), founder: a.party.ipk, state: None }),
        )
        .unwrap();
        let (_, welcome) =
            ga.add_members(&a.provider, &a.party.sig_kp, &[b.legacy_kp(), c.legacy_kp()]).unwrap();
        ga.merge_pending_commit(&a.provider).unwrap();
        // These seats were joined by the old client, before credential binding.
        for r in [&b, &c] {
            StagedWelcome::new_from_welcome(
                &r.provider,
                &MlsGroupJoinConfig::builder().use_ratchet_tree_extension(true).build(),
                extract_welcome_via_tls(welcome.clone()),
                None,
            )
            .unwrap()
            .into_group(&r.provider)
            .unwrap();
        }
        let source = Source::read(&ga).unwrap().unwrap();
        let mut approvals = [&a, &b, &c]
            .map(|r| source.approve(&r.group(&gid), &r.party.ipk_signer).unwrap())
            .to_vec();
        approvals.sort_by_key(|a| a.who.0);
        (gid, a, b, c, approvals)
    }

    fn invitation(a: &Replica, target: [u8; 32], who: [u8; 32]) -> MlsEnvelopeP {
        let frame: Vec<u8> = a
            .provider
            .storage()
            .connection()
            .lock()
            .query_row(
                "SELECT frame FROM mls_dispatch_jobs WHERE group_id=?1 AND recipient=?2",
                rusqlite::params![target, who],
                |r| r.get(0),
            )
            .unwrap();
        let common::proto::client_rel::CRelayPacket::Dispatch(dispatch) =
            common::proto::client_rel::CRelayPacket::deser(&frame[4..]).unwrap()
        else {
            panic!()
        };
        MlsEnvelopeP::deser(&dispatch.payload).unwrap()
    }

    fn accept(r: &Replica, from: [u8; 32], envelope: MlsEnvelopeP) -> anyhow::Result<[u8; 32]> {
        let MlsEnvelopeP::GroupMigrationWelcome {
            group,
            branch,
            approvals,
            welcome,
            history,
            signature,
        } = envelope
        else {
            panic!()
        };
        migration::accept(
            &r.provider,
            group.0,
            branch.0,
            [r.party.ipk[0]; 16],
            r.party.ipk,
            from,
            &approvals,
            &welcome,
            &history.0,
            &signature.0,
        )
    }

    #[test]
    fn migration_requires_every_identity_and_survives_failed_publication_and_redelivery() {
        let (gid, a, b, c, approvals) = fixture();
        let packages = vec![b.fresh(), c.fresh()];
        let source = Source::read(&a.group(&gid)).unwrap().unwrap();
        let target = source.target();
        assert!(
            migration::create(
                &a.provider,
                gid,
                [71; 16],
                &a.party.ipk_signer,
                &approvals[..2],
                &packages
            )
            .is_err()
        );
        let mut forged = approvals.clone();
        forged[1].signature = forged[0].signature;
        assert!(
            migration::create(&a.provider, gid, [71; 16], &a.party.ipk_signer, &forged, &packages)
                .is_err()
        );
        assert!(
            migration::create(
                &a.provider,
                gid,
                [71; 16],
                &b.party.ipk_signer,
                &approvals,
                &packages
            )
            .is_err()
        );
        a.provider.storage().connection().lock().execute_batch(
            "CREATE TEMP TRIGGER fail_migration BEFORE INSERT ON mls_group_migrations BEGIN SELECT RAISE(ABORT,'disk failure'); END").unwrap();
        assert!(
            migration::create(
                &a.provider,
                gid,
                [71; 16],
                &a.party.ipk_signer,
                &approvals,
                &packages
            )
            .is_err()
        );
        assert_eq!(a.group(&gid).branch_id(), source.branch);
        assert!(MlsGroupHandle::load(&a.provider, &target).unwrap().is_none());
        assert_eq!(
            a.provider
                .storage()
                .connection()
                .lock()
                .query_row("SELECT COUNT(*) FROM mls_dispatch_jobs", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
        a.provider
            .storage()
            .connection()
            .lock()
            .execute_batch("DROP TRIGGER fail_migration")
            .unwrap();
        assert_eq!(
            migration::create(
                &a.provider,
                gid,
                [71; 16],
                &a.party.ipk_signer,
                &approvals,
                &packages
            )
            .unwrap(),
            target
        );
        assert!(MlsGroupHandle::load(&a.provider, &gid).unwrap().is_none());
        // Reload persisted stores; the mapping and both invitations survive.
        let restarted = PromtuzMlsProvider::new(a.provider.storage().connection());
        assert_eq!(migration::completed(&restarted, &gid).unwrap(), Some((target, [71; 16])));
        let b_invite = invitation(&a, target, b.party.ipk);
        let mut invalid = b_invite.clone();
        if let MlsEnvelopeP::GroupMigrationWelcome {
            group,
            branch,
            approvals,
            welcome,
            history,
            signature,
        } = &mut invalid
        {
            history.0[0] ^= 1;
            *signature = a
                .party
                .ipk_signer
                .sign(&common::proto::mls_wire::group_migration_welcome_signing_input(
                    &group.0, &branch.0, approvals, welcome, &history.0,
                ))
                .to_bytes()
                .into();
        }
        assert!(accept(&b, a.party.ipk, invalid).is_err());
        assert_eq!(b.group(&gid).branch_id(), source.branch);
        b.provider.storage().connection().lock().execute_batch(
            "CREATE TEMP TRIGGER fail_join BEFORE INSERT ON mls_branches BEGIN SELECT RAISE(ABORT,'disk failure after consuming KP'); END",
        ).unwrap();
        assert!(accept(&b, a.party.ipk, b_invite.clone()).is_err());
        assert_eq!(b.group(&gid).branch_id(), source.branch);
        b.provider.storage().connection().lock().execute_batch("DROP TRIGGER fail_join").unwrap();
        assert_eq!(
            accept(&b, a.party.ipk, b_invite.clone()).unwrap(),
            target,
            "bad invitation did not consume keys"
        );
        assert_eq!(accept(&b, a.party.ipk, b_invite).unwrap(), target, "redelivery is idempotent");
        accept(&c, a.party.ipk, invitation(&a, target, c.party.ipk)).unwrap();
        let mut ga = a.group(&target);
        for r in [&a, &b, &c] {
            assert_eq!(r.group(&target).branch_id(), ga.branch_id());
            assert!(r.group(&target).group_meta().unwrap().state.is_some());
            assert_eq!(r.group(&target).roster().len(), 3);
        }
        let signer =
            crate::messaging::leaf_signer_for_group(&a.provider, &ga, &a.party.ipk).unwrap();
        let msg = ga
            .create_application_message(&a.provider, &signer, b"still the same conversation")
            .unwrap();
        for r in [&b, &c] {
            let mut group = r.group(&target);
            let decoded = group
                .process_incoming(
                    &r.provider,
                    mls_message_from_bytes(&mls_message_to_bytes(&msg).unwrap())
                        .unwrap()
                        .try_into_protocol_message()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(decoded.sender, a.party.ipk);
            let ProcessedMessageContent::ApplicationMessage(body) = decoded.content else {
                panic!()
            };
            assert_eq!(body.into_bytes(), b"still the same conversation");
        }
    }

    #[test]
    fn migration_rejects_a_different_epoch_and_preserves_the_old_session() {
        let (gid, a, b, c, approvals) = fixture();
        let target = migration::create(
            &a.provider,
            gid,
            [71; 16],
            &a.party.ipk_signer,
            &approvals,
            &[b.fresh(), c.fresh()],
        )
        .unwrap();
        let mut gb = b.group(&gid);
        gb.self_update(&b.provider, &b.party.sig_kp).unwrap();
        gb.merge_pending_commit(&b.provider).unwrap();
        let advanced = gb.branch_id();
        assert!(accept(&b, a.party.ipk, invitation(&a, target, b.party.ipk)).is_err());
        assert_eq!(b.group(&gid).branch_id(), advanced);
        assert!(MlsGroupHandle::load(&b.provider, &target).unwrap().is_none());
    }

    #[test]
    fn legacy_direct_credentials_still_send_without_group_migration() {
        let a = Replica::new(74);
        let b = Replica::new(75);
        let gid = rand::random();
        let mut group =
            MlsGroupHandle::create(&a.provider, &a.party.sig_kp, a.credential(), &gid, None)
                .unwrap();
        let (_, welcome) =
            group.add_members(&a.provider, &a.party.sig_kp, &[b.legacy_kp()]).unwrap();
        group.merge_pending_commit(&a.provider).unwrap();
        let mut other = MlsGroupHandle::wrap(
            StagedWelcome::new_from_welcome(
                &b.provider,
                &MlsGroupJoinConfig::builder().use_ratchet_tree_extension(true).build(),
                extract_welcome_via_tls(welcome),
                None,
            )
            .unwrap()
            .into_group(&b.provider)
            .unwrap(),
        );
        assert!(Source::read(&group).unwrap().is_none());
        let signer =
            crate::messaging::leaf_signer_for_group(&a.provider, &group, &a.party.ipk).unwrap();
        let msg = group.create_application_message(&a.provider, &signer, b"legacy pair").unwrap();
        let received = other
            .process_incoming(
                &b.provider,
                mls_message_from_bytes(&mls_message_to_bytes(&msg).unwrap())
                    .unwrap()
                    .try_into_protocol_message()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(received.sender, a.party.ipk);
    }

    #[test]
    fn migration_projection_preserves_content_and_recovers_a_failed_database_switch() {
        const CHILD: &str = "PROMTUZ_MIGRATION_PROJECTION_TEST";
        if std::env::var_os(CHILD).is_none() {
            let dir = std::env::temp_dir()
                .join(format!("promtuz-migration-projection-{}", ulid::Ulid::new()));
            std::fs::create_dir_all(&dir).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap()).args([
                "--exact", "mls::group::tests::legacy_migration::migration_projection_preserves_content_and_recovers_a_failed_database_switch", "--nocapture",
            ]).env(CHILD,"1").env("PROMTUZ_DATA_DIR",&dir).output().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        use rusqlite::params;

        use crate::data::conversation::Conversation;
        use crate::db::messages::MESSAGES_DB;
        let (gid, a, b, c, approvals) = fixture();
        let conversation = [71; 16];
        let did = [72u8; 16];
        let message = ulid::Ulid::new().to_string();
        {
            let conn = MESSAGES_DB.lock();
            conn.execute("INSERT INTO conversations(id,kind,created_by,mls_group_id,title,pinned,muted) VALUES(?1,1,?2,?3,'Saved name',1,1)",
                params![conversation,a.party.ipk,gid]).unwrap();
            conn.execute("INSERT INTO messages(id,conversation_id,content,outgoing,timestamp,status,dispatch_id) VALUES(?1,?2,'Keep this attachment',0,42,1,?3)",
                params![message,conversation,did]).unwrap();
            conn.execute("INSERT INTO message_media(conversation_id,dispatch_id,kind,mime,blob,file_id) VALUES(?1,?2,1,'image/avif',?3,?4)",
                params![conversation,did,vec![7u8;128],vec![9u8;32]]).unwrap();
        }
        let target = migration::create(
            &a.provider,
            gid,
            conversation,
            &a.party.ipk_signer,
            &approvals,
            &[b.fresh(), c.fresh()],
        )
        .unwrap();
        MESSAGES_DB.lock().execute_batch("CREATE TEMP TRIGGER fail_projection BEFORE UPDATE ON conversations BEGIN SELECT RAISE(ABORT,'disk failure'); END").unwrap();
        assert!(crate::groups::migration::finish(&a.provider, gid).is_err());
        assert_eq!(Conversation::group_of(&conversation), Some(gid));
        MESSAGES_DB.lock().execute_batch("DROP TRIGGER fail_projection").unwrap();
        crate::groups::migration::finish(&a.provider, gid).unwrap();
        crate::groups::migration::finish(&a.provider, gid).unwrap();
        assert_eq!(Conversation::group_of(&conversation), Some(target));
        assert_eq!(Conversation::active_members(&conversation).len(), 3);
        assert!(Conversation::has_signed_rules(&conversation));
        assert!(crate::groups::migration::retired(&a.provider, &gid).unwrap());
        assert!(
            crate::groups::migration::retired(&build_provider(), &gid).unwrap(),
            "the backup-preserved marker also retires the old session without MLS storage"
        );
        let conn = MESSAGES_DB.lock();
        assert_eq!(
            conn.query_row(
                "SELECT title,pinned,muted FROM conversations WHERE id=?1",
                [conversation],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?))
            )
            .unwrap(),
            ("Saved name".into(), true, true)
        );
        assert_eq!(
            conn.query_row("SELECT content FROM messages WHERE id=?1", [message], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "Keep this attachment"
        );
        assert_eq!(
            conn.query_row(
                "SELECT blob,file_id FROM message_media WHERE conversation_id=?1",
                [conversation],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            )
            .unwrap(),
            (vec![7; 128], vec![9; 32])
        );
        conn.execute("DELETE FROM conversations WHERE id=?1", [conversation]).unwrap();
        drop(conn);
        assert!(crate::groups::migration::destination(&a.provider, &target).is_err());
        assert!(
            Conversation::get(&conversation).is_none(),
            "late migration cannot recreate a deleted chat"
        );
    }
}
