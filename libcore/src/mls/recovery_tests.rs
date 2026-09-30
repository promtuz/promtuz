// These tests use independent MLS databases and genuine founder-signed
// histories. Assertions concern convergence, authority and recoverability.
mod recovery_protocol {
    use common::proto::mls_wire::GroupBranch;
    use common::proto::mls_wire::GroupMemberAction;
    use common::proto::mls_wire::GroupMemberRequest;
    use common::proto::mls_wire::WelcomeEnvelopeP;
    use ed25519_dalek::Signer;

    use super::*;
    use crate::mls::branch_proof as proof;
    use crate::mls::recovery as journal;

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
        fn kp(&self) -> KeyPackage {
            make_kp(&self.provider, &self.party)
        }
        fn group(&self, gid: &[u8; 32]) -> MlsGroupHandle {
            MlsGroupHandle::load(&self.provider, gid).unwrap().unwrap()
        }
        fn request(&self, gid: &[u8; 32], action: GroupMemberAction) -> GroupMemberRequest {
            let nonce = [self.party.ipk[0]; 16];
            let sig = self.party.ipk_signer.sign(
                &common::proto::mls_wire::group_member_request_signing_input(
                    gid,
                    &self.party.ipk,
                    &nonce,
                    &action,
                ),
            );
            GroupMemberRequest {
                who: self.party.ipk.into(),
                nonce: nonce.into(),
                action,
                signature: sig.to_bytes().into(),
            }
        }
    }

    struct Commit {
        parent:            [u8; 32],
        bytes:             Vec<u8>,
        proof:             GroupBranch,
        history:           Vec<GroupBranch>,
        welcome:           Option<MlsMessageOut>,
        encrypted_history: Vec<u8>,
    }
    fn commit(
        r: &Replica, gid: [u8; 32], change: Option<GroupChange>, adds: Vec<KeyPackage>,
    ) -> anyhow::Result<Commit> {
        let mut tx = journal::Transaction::open(&r.provider, gid, None)?.unwrap();
        let parent = tx.parent;
        let before = tx.group.group_meta().unwrap().effective();
        let signed = change.map(|c| signed_by(&tx.group, &r.party, c));
        let (message, welcome) = if let Some(signed) = &signed {
            let mut meta = tx.group.group_meta().unwrap();
            meta.state = Some(
                tx.group.state_after_change(&r.party.ipk, signed).map_err(anyhow::Error::msg)?,
            );
            let removed = match &signed.change {
                GroupChange::MemberRequest(request) => Some(request.who.0),
                GroupChange::Remove { who } => Some(who.0),
                _ => None,
            };
            let removes = removed
                .map(|who| tx.group.member_index_by_ipk(&who).unwrap())
                .into_iter()
                .collect();
            tx.group.commit_meta(&tx.provider, &r.party.sig_kp, &meta, adds, removes)?
        } else {
            let (message, welcome) = tx.group.add_members(&tx.provider, &r.party.sig_kp, &adds)?;
            (message, Some(welcome))
        };
        let bytes = mls_message_to_bytes(&message)?;
        tx.group.merge_pending_commit(&tx.provider)?;
        let proof = proof::sign(
            &tx.group,
            Some(parent),
            before.role(&r.party.ipk),
            &bytes,
            &r.party.ipk_signer,
        )?;
        let history = journal::next_history(&r.provider, &gid, parent, &proof)?;
        proof::verify_history(&gid, &history, &tx.group)?;
        let encrypted_history = proof::seal(
            &tx.provider,
            &tx.group,
            proof::INVITATION_LABEL,
            &postcard::to_allocvec(&history)?,
        )?;
        tx.publish(
            Some(journal::Candidate {
                rank:    proof.rank,
                message: bytes.clone(),
                change:  signed,
                proof:   Some(proof.clone()),
            }),
            &[],
            None,
            None,
        )?;
        Ok(Commit { parent, bytes, proof, history, welcome, encrypted_history })
    }

    fn receive(r: &Replica, gid: [u8; 32], commit: &Commit) -> anyhow::Result<()> {
        let mut tx = journal::Transaction::open(&r.provider, gid, Some(commit.parent))?.unwrap();
        let before = MlsGroupHandle::load(&tx.provider, &gid)?.unwrap();
        let processed = tx.group.process_incoming(
            &tx.provider,
            mls_message_from_bytes(&commit.bytes)?.try_into_protocol_message().unwrap(),
        )?;
        let author = processed.sender;
        let CommitOutcome::Merged(changed) = tx.group.merge_staged_commit_if_permitted(
            &tx.provider,
            commit_of(processed.content),
            author,
        )?
        else {
            anyhow::bail!("commit refused");
        };
        proof::verify_commit(&before, &tx.group, &commit.proof, &author, &commit.bytes)?;
        let history = journal::next_history(&r.provider, &gid, commit.parent, &commit.proof)?;
        proof::verify_history(&gid, &history, &tx.group)?;
        tx.publish(
            Some(journal::Candidate {
                rank:    commit.proof.rank,
                message: commit.bytes.clone(),
                change:  changed.map(|c| c.signed),
                proof:   Some(commit.proof.clone()),
            }),
            &[],
            None,
            None,
        )?;
        Ok(())
    }

    fn invitation(
        from: &Replica, to: &Replica, gid: [u8; 32], kp: &KeyPackage, c: &Commit,
    ) -> WelcomeEnvelopeP {
        let reference: [u8; 32] =
            kp.hash_ref(from.provider.crypto()).unwrap().as_slice().try_into().unwrap();
        crate::mls::make_welcome_envelope(
            c.welcome.clone().unwrap(),
            gid,
            from.party.ipk,
            to.party.ipk,
            reference,
            &from.party.ipk_signer,
        )
        .unwrap()
    }

    fn founded() -> ([u8; 32], Replica, Replica, Replica) {
        let a = Replica::new(101);
        let b = Replica::new(102);
        let c = Replica::new(103);
        let gid = [201; 32];
        let group = MlsGroupHandle::create(
            &a.provider,
            &a.party.sig_kp,
            a.party.cwk(),
            &gid,
            Some(&GroupMeta::founded("Group".into(), a.party.ipk)),
        )
        .unwrap();
        journal::ensure_root(&a.provider, &group, &a.party.ipk_signer).unwrap();
        let kb = b.kp();
        let kc = c.kp();
        let initial = commit(&a, gid, None, vec![kb.clone(), kc.clone()]).unwrap();
        for (to, kp) in [(&b, kb), (&c, kc)] {
            journal::accept_welcome(
                &to.provider,
                &invitation(&a, to, gid, &kp, &initial),
                &initial.encrypted_history,
                None,
                None,
            )
            .unwrap();
        }
        for to in [&b, &c] {
            let promotion = commit(
                &a,
                gid,
                Some(GroupChange::Role { who: to.party.ipk.into(), role: policy::ROLE_ADMIN }),
                vec![],
            )
            .unwrap();
            receive(&b, gid, &promotion).unwrap();
            receive(&c, gid, &promotion).unwrap();
        }
        (gid, a, b, c)
    }

    #[test]
    fn signed_takeovers_and_losing_joiner_converge_without_exposing_prejoin_keys() {
        let (gid, a, b, c) = founded();
        let b_commit = commit(&b, gid, Some(GroupChange::Takeover), vec![]).unwrap();
        let c_commit = commit(&c, gid, Some(GroupChange::Takeover), vec![]).unwrap();
        let (winner, winning, loser, losing) =
            if b_commit.proof.commit_hash.0 < c_commit.proof.commit_hash.0 {
                (&b, &b_commit, &c, &c_commit)
            } else {
                (&c, &c_commit, &b, &b_commit)
            };
        let joiner = Replica::new(104);
        let old_kp = joiner.kp();
        let lost_add = commit(
            loser,
            gid,
            Some(GroupChange::Add { who: vec![joiner.party.ipk.into()] }),
            vec![old_kp.clone()],
        )
        .unwrap();
        let old_invite = invitation(loser, &joiner, gid, &old_kp, &lost_add);
        journal::accept_welcome(
            &joiner.provider,
            &old_invite,
            &lost_add.encrypted_history,
            None,
            None,
        )
        .unwrap();
        receive(&a, gid, losing).unwrap();
        receive(&a, gid, &lost_add).unwrap();
        receive(&a, gid, winning).unwrap();
        receive(winner, gid, losing).unwrap();
        receive(winner, gid, &lost_add).unwrap();
        receive(loser, gid, winning).unwrap();
        for r in [&a, &b, &c] {
            assert_eq!(r.group(&gid).branch_id(), winner.group(&gid).branch_id());
        }

        // Original inviter forwards a canonical invitation carried by the
        // winning committer. Neither inviter can forge the committer's proof.
        let new_kp = joiner.kp();
        let added = commit(
            winner,
            gid,
            Some(GroupChange::Add { who: vec![joiner.party.ipk.into()] }),
            vec![new_kp.clone()],
        )
        .unwrap();
        let replacement = invitation(loser, &joiner, gid, &new_kp, &added);
        let mut forged = added.history.clone();
        forged[1].rank = u8::MAX;
        let forged_bytes = proof::seal(
            &winner.provider,
            &winner.group(&gid),
            proof::INVITATION_LABEL,
            &postcard::to_allocvec(&forged).unwrap(),
        )
        .unwrap();
        let old_branch = joiner.group(&gid).branch_id();
        assert!(
            journal::accept_welcome(&joiner.provider, &replacement, &forged_bytes, None, None)
                .is_err()
        );
        assert_eq!(
            joiner.group(&gid).branch_id(),
            old_branch,
            "invalid history cannot replace live keys"
        );
        let accepted = journal::accept_welcome(
            &joiner.provider,
            &replacement,
            &added.encrypted_history,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            accepted.branch_id(),
            winner.group(&gid).branch_id(),
            "rejected invitation did not consume the fresh KP"
        );
        assert!(
            journal::Transaction::open(&joiner.provider, gid, Some(winning.parent))
                .unwrap()
                .is_none(),
            "joiner never has pre-join secrets"
        );
        assert!(
            journal::accept_welcome(
                &joiner.provider,
                &old_invite,
                &lost_add.encrypted_history,
                None,
                None
            )
            .is_err(),
            "late losing invitation cannot roll back recovery"
        );
    }

    #[test]
    fn refresh_after_secret_retirement_preserves_identity_and_rejects_repeated_nonce() {
        let (gid, a, b, c) = founded();
        let request = c.request(&gid, GroupMemberAction::Refresh);
        let anchor =
            journal::history(&a.provider, &gid, a.group(&gid).branch_id()).unwrap()[0].branch.0;
        let old = c.group(&gid).branch_id();
        let rules = commit(
            &a,
            gid,
            Some(GroupChange::Rules(GroupRules { members_send: false, ..GroupRules::default() })),
            vec![],
        )
        .unwrap();
        receive(&b, gid, &rules).unwrap();
        journal::prune(&a.provider, &gid, crate::utils::systime().as_secs() + 8 * 86400).unwrap();
        assert!(journal::Transaction::open(&a.provider, gid, Some(old)).unwrap().is_none());
        let kp = c.kp();
        let refreshed =
            commit(&a, gid, Some(GroupChange::MemberRequest(request.clone())), vec![kp.clone()])
                .unwrap();
        let env = invitation(&a, &c, gid, &kp, &refreshed);
        assert!(
            journal::accept_welcome(
                &c.provider,
                &env,
                &refreshed.encrypted_history,
                Some(&request),
                Some([9; 32])
            )
            .is_err()
        );
        let group = journal::accept_welcome(
            &c.provider,
            &env,
            &refreshed.encrypted_history,
            Some(&request),
            Some(anchor),
        )
        .unwrap();
        assert_eq!(group.branch_id(), a.group(&gid).branch_id());
        assert!(!group.group_meta().unwrap().effective().rules.members_send);
        let epoch = a.group(&gid).epoch();
        assert!(commit(&a, gid, Some(GroupChange::MemberRequest(request)), vec![c.kp()]).is_err());
        assert_eq!(a.group(&gid).epoch(), epoch, "nonce replay must not publish a new epoch");
    }

    #[test]
    fn signed_departure_survives_epoch_changes_and_preserves_a_remaining_owner() {
        let (gid, a, b, c) = founded();
        let leave = a.request(&gid, GroupMemberAction::Leave);
        let takeover = commit(&b, gid, Some(GroupChange::Takeover), vec![]).unwrap();
        receive(&c, gid, &takeover).unwrap();
        receive(&a, gid, &takeover).unwrap();
        let departed =
            commit(&b, gid, Some(GroupChange::MemberRequest(leave.clone())), vec![]).unwrap();
        receive(&c, gid, &departed).unwrap();
        // Removed members can validate the branch even without the new epoch
        // secrets; its identity comes from the public tree and extensions.
        receive(&a, gid, &departed).unwrap();
        for r in [&a, &b, &c] {
            assert_eq!(r.group(&gid).branch_id(), b.group(&gid).branch_id());
        }
        let state = b.group(&gid).group_meta().unwrap().effective();
        assert_eq!(state.owners, vec![b.party.ipk]);
        assert!(!b.group(&gid).roster().contains(&a.party.ipk));
        let mut forged = leave;
        forged.who = c.party.ipk.into();
        assert!(proof::verify_member_request(&gid, &forged).is_err());
    }

    #[test]
    fn another_admin_can_restore_the_committer_after_it_loses_all_epoch_keys() {
        let (gid, a, b, c) = founded();
        let request = a.request(&gid, GroupMemberAction::Refresh);
        let anchor =
            journal::history(&b.provider, &gid, b.group(&gid).branch_id()).unwrap()[0].branch.0;
        let kp = a.kp();
        let recovered =
            commit(&b, gid, Some(GroupChange::MemberRequest(request.clone())), vec![kp.clone()])
                .unwrap();
        receive(&c, gid, &recovered).unwrap();
        a.provider.storage().forget_group(&gid).unwrap();
        let group = journal::accept_welcome(
            &a.provider,
            &invitation(&b, &a, gid, &kp, &recovered),
            &recovered.encrypted_history,
            Some(&request),
            Some(anchor),
        )
        .unwrap();
        assert_eq!(group.branch_id(), b.group(&gid).branch_id());
        assert_eq!(group.group_meta().unwrap().effective().owners, vec![a.party.ipk]);
        assert_eq!(group.group_meta().unwrap().effective().committer, b.party.ipk);
    }

    #[test]
    fn live_ingress_rejects_substituted_proof_without_consuming_the_valid_commit() {
        // Live ingress uses process-wide stores and launches follow-up work.
        // Keep those stores separate from the other tests' identities and groups.
        const CHILD: &str = "PROMTUZ_GROUP_INGRESS_TEST";
        if std::env::var_os(CHILD).is_none() {
            let dir = std::env::temp_dir().join(format!(
                "promtuz-group-ingress-{}-{}",
                std::process::id(),
                ulid::Ulid::new(),
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(module_path!(), "::live_ingress_rejects_substituted_proof_without_consuming_the_valid_commit")
                        .strip_prefix("core::").unwrap(),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("PROMTUZ_DATA_DIR", &dir)
                .output()
                .unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(result.status.success(), "{}\n{}",
                String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        }
        use common::proto::mls_wire::MlsApplicationEnvelopeP;
        use common::proto::mls_wire::group_envelope_signing_input;
        let (gid, a, b, c) = founded();
        let conversation = [211; 16];
        {
            let db = crate::db::messages::MESSAGES_DB.lock();
            db.execute(
                "INSERT INTO conversations(id,kind,created_by,mls_group_id) VALUES(?1,1,?2,?3)",
                rusqlite::params![conversation, a.party.ipk, gid],
            )
            .unwrap();
        }
        crate::data::conversation::Conversation::sync_group(
            &conversation,
            &c.group(&gid).roster(),
            c.group(&gid).group_meta().as_ref(),
        )
        .unwrap();
        let before = a.group(&gid);
        let change = commit(
            &a,
            gid,
            Some(GroupChange::Rules(GroupRules { members_send: false, ..GroupRules::default() })),
            vec![],
        )
        .unwrap();
        let transcript = group_envelope_signing_input(
            common::PROTOCOL_VERSION,
            &c.party.ipk,
            &gid,
            before.epoch(),
            &change.parent,
            &change.bytes,
        );
        let envelope = MlsApplicationEnvelopeP {
            version:     common::proto::mls_wire::MLS_ENVELOPE_VERSION,
            group_id:    gid.into(),
            epoch:       before.epoch(),
            mls_message: change.bytes.clone().into(),
            sender_sig:  a.party.ipk_signer.sign(&transcript).to_bytes().into(),
        };
        let seal = |p: &GroupBranch| {
            let parent =
                journal::Transaction::open(&a.provider, gid, Some(change.parent)).unwrap().unwrap();
            proof::seal(
                &parent.provider,
                &parent.group,
                proof::COMMIT_LABEL,
                &postcard::to_allocvec(&(None::<GroupBranch>, p)).unwrap(),
            )
            .unwrap()
        };
        let mut forged = change.proof.clone();
        forged.author = b.party.ipk.into();
        assert!(
            crate::groups::recovery::receive(
                &c.provider,
                &c.party.ipk,
                a.party.ipk,
                change.parent,
                envelope.clone(),
                Some(seal(&forged)),
                [1; 16],
                1000
            )
            .is_err()
        );
        assert_eq!(c.group(&gid).branch_id(), before.branch_id());
        crate::groups::recovery::receive(
            &c.provider,
            &c.party.ipk,
            a.party.ipk,
            change.parent,
            envelope,
            Some(seal(&change.proof)),
            [1; 16],
            1000,
        )
        .unwrap();
        assert_eq!(c.group(&gid).branch_id(), a.group(&gid).branch_id());
        assert!(
            !crate::data::conversation::Conversation::state(&conversation)
                .unwrap()
                .rules
                .members_send
        );

        // A correctly signed retired envelope still cannot bypass the journal.
        let stash = crate::mls::KeyPackageStash::new(c.provider.storage().connection());
        let buffer = crate::mls::EpochCatchupBuffer::new(c.provider.storage().connection());
        let dht = crate::quic::dht_client::NotWiredDhtClient;
        let ctx = crate::messaging::MlsContext {
            provider: &c.provider,
            stash:    &stash,
            buffer:   &buffer,
            dht:      &dht,
        };
        let old = crate::messaging::SealedMessage {
            group_id:  gid,
            epoch:     a.group(&gid).epoch(),
            branch:    None,
            mls_bytes: vec![],
            proof:     None,
        };
        use common::proto::mls_wire::MlsEnvelopeP;
        use common::proto::pack::Unpacker;
        let MlsEnvelopeP::Application(old) =
            MlsEnvelopeP::deser(&old.address_to(&c.party.ipk, &a.party.ipk_signer).unwrap())
                .unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            crate::messaging::process_application_inbound_for(
                &ctx,
                a.party.ipk,
                &c.party.ipk,
                old,
                1000,
                [2; 16]
            )
            .unwrap(),
            crate::messaging::InboundDecoded::ApplicationStale
        ));
    }
}
