//! Signed-group recovery across independent devices with genuine founder-signed histories.

use anyhow::Result;
use common::proto::mls_wire::GroupBranch;
use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::GroupMemberAction;
use common::proto::mls_wire::GroupMemberRequest;
use common::proto::mls_wire::GroupRules;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::group_member_request_signing_input;
use ed25519_dalek::Signer as _;
use openmls::prelude::KeyPackage;
use openmls::prelude::MlsMessageOut;
use openmls::prelude::tls_codec::Serialize as _;

use super::CommitOutcome;
use super::GroupMeta;
use super::MlsGroupHandle;
use super::branch_proof as proof;
use super::policy;
use super::recovery as journal;
use crate::test_support::mls::*;

fn request(who: &Party, gid: &[u8; 32], action: GroupMemberAction) -> GroupMemberRequest {
    let nonce = [who.ipk[0]; 16];
    let input = group_member_request_signing_input(gid, &who.ipk, &nonce, &action);
    GroupMemberRequest {
        who: who.ipk.into(),
        nonce: nonce.into(),
        action,
        signature: who.identity.sign(&input).to_bytes().into(),
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

/// `by` commits `change` with `adds` on its live head, or only the adds when there is no change.
fn commit(
    by: &Party, gid: [u8; 32], change: Option<GroupChange>, adds: Vec<KeyPackage>,
) -> Result<Commit> {
    let mut tx = journal::Transaction::open(&by.provider, gid, None)?.unwrap();
    let parent = tx.parent;
    let before = tx.group.group_meta().unwrap().effective();
    let signed = change.map(|c| by.sign(&tx.group, c));
    let (message, welcome) = if let Some(signed) = &signed {
        let mut meta = tx.group.group_meta().unwrap();
        meta.state =
            Some(tx.group.state_after_change(&by.ipk, signed).map_err(anyhow::Error::msg)?);
        let removed = match &signed.change {
            GroupChange::MemberRequest(request) => Some(request.who.0),
            GroupChange::Remove { who } => Some(who.0),
            _ => None,
        };
        let removes = removed.map(|who| tx.group.member_index_by_ipk(&who).unwrap());
        tx.group.commit_meta(&tx.provider, &by.leaf, &meta, adds, removes.into_iter().collect())?
    } else {
        let (message, welcome) = tx.group.add_members(&tx.provider, &by.leaf, &adds)?;
        (message, Some(welcome))
    };
    let bytes = message.tls_serialize_detached()?;
    tx.group.merge_pending_commit(&tx.provider)?;
    let proof = proof::sign(&tx.group, Some(parent), before.role(&by.ipk), &bytes, &by.identity)?;
    let history = journal::next_history(&by.provider, &gid, parent, &proof)?;
    proof::verify_history(&gid, &history, &tx.group)?;
    let sealed = postcard::to_allocvec(&history)?;
    let encrypted_history = proof::seal(&tx.provider, &tx.group, proof::INVITATION_LABEL, &sealed)?;
    let candidate = journal::Candidate {
        rank:    proof.rank,
        message: bytes.clone(),
        change:  signed,
        proof:   Some(proof.clone()),
    };
    tx.publish(Some(candidate), &[], None, None)?;
    Ok(Commit { parent, bytes, proof, history, welcome, encrypted_history })
}

fn receive(by: &Party, gid: [u8; 32], commit: &Commit) -> Result<()> {
    let mut tx = journal::Transaction::open(&by.provider, gid, Some(commit.parent))?.unwrap();
    let before = MlsGroupHandle::load(&tx.provider, &gid)?.unwrap();
    let protocol = super::group::mls_message_from_bytes(&commit.bytes)?;
    let processed =
        tx.group.process_incoming(&tx.provider, protocol.try_into_protocol_message()?)?;
    let author = processed.sender;
    let staged = commit_of(processed.content);
    let CommitOutcome::Merged(changed) =
        tx.group.merge_staged_commit_if_permitted(&tx.provider, staged, author)?
    else {
        anyhow::bail!("commit refused");
    };
    proof::verify_commit(&before, &tx.group, &commit.proof, &author, &commit.bytes)?;
    let history = journal::next_history(&by.provider, &gid, commit.parent, &commit.proof)?;
    proof::verify_history(&gid, &history, &tx.group)?;
    let candidate = journal::Candidate {
        rank:    commit.proof.rank,
        message: commit.bytes.clone(),
        change:  changed.map(|c| c.signed),
        proof:   Some(commit.proof.clone()),
    };
    tx.publish(Some(candidate), &[], None, None)?;
    Ok(())
}

fn invitation(
    from: &Party, to: &Party, gid: [u8; 32], kp: &KeyPackage, c: &Commit,
) -> WelcomeEnvelopeP {
    from.envelope(to, gid, c.welcome.clone().unwrap(), kp)
}

/// Alice founds a signed group of three and makes Bob and Carol admins.
fn founded() -> ([u8; 32], Party, Party, Party) {
    let (a, b, c) = (Party::new(101), Party::new(102), Party::new(103));
    let gid = [201; 32];
    let meta = GroupMeta::founded("Group".into(), a.ipk);
    let group =
        MlsGroupHandle::create(&a.provider, &a.leaf, a.credential(), &gid, Some(&meta)).unwrap();
    journal::ensure_root(&a.provider, &group, &a.identity).unwrap();
    let (kb, kc) = (b.kp(), c.kp());
    let initial = commit(&a, gid, None, vec![kb.clone(), kc.clone()]).unwrap();
    for (to, kp) in [(&b, kb), (&c, kc)] {
        let env = invitation(&a, to, gid, &kp, &initial);
        journal::accept_welcome(&to.provider, &env, &initial.encrypted_history, None, None)
            .unwrap();
    }
    for to in [&b, &c] {
        let role = GroupChange::Role { who: to.ipk.into(), role: policy::ROLE_ADMIN };
        let promotion = commit(&a, gid, Some(role), vec![]).unwrap();
        receive(&b, gid, &promotion).unwrap();
        receive(&c, gid, &promotion).unwrap();
    }
    (gid, a, b, c)
}

/// A joiner invited on the losing branch converges with everyone else, never holds keys from
/// before it joined, and keeps its KeyPackage through a forged or late invitation.
#[test]
fn signed_takeovers_and_losing_joiner_converge_without_exposing_prejoin_keys() {
    let (gid, a, b, c) = founded();
    let b_commit = commit(&b, gid, Some(GroupChange::Takeover), vec![]).unwrap();
    let c_commit = commit(&c, gid, Some(GroupChange::Takeover), vec![]).unwrap();
    let ((winner, winning), (loser, losing)) =
        if b_commit.proof.commit_hash.0 < c_commit.proof.commit_hash.0 {
            ((&b, &b_commit), (&c, &c_commit))
        } else {
            ((&c, &c_commit), (&b, &b_commit))
        };
    let joiner = Party::new(104);
    let old_kp = joiner.kp();
    let add_joiner = || GroupChange::Add { who: vec![joiner.ipk.into()] };
    let lost_add = commit(loser, gid, Some(add_joiner()), vec![old_kp.clone()]).unwrap();
    let old_invite = invitation(loser, &joiner, gid, &old_kp, &lost_add);
    let accept = |env: &WelcomeEnvelopeP, history: &[u8]| {
        journal::accept_welcome(&joiner.provider, env, history, None, None)
    };
    accept(&old_invite, &lost_add.encrypted_history).unwrap();
    receive(&a, gid, losing).unwrap();
    receive(&a, gid, &lost_add).unwrap();
    receive(&a, gid, winning).unwrap();
    receive(winner, gid, losing).unwrap();
    receive(winner, gid, &lost_add).unwrap();
    receive(loser, gid, winning).unwrap();
    for r in [&a, &b, &c] {
        assert_eq!(r.group(&gid).branch_id(), winner.group(&gid).branch_id());
    }

    // The original inviter forwards the winning committer's invitation; no inviter can forge the
    // committer's proof.
    let new_kp = joiner.kp();
    let added = commit(winner, gid, Some(add_joiner()), vec![new_kp.clone()]).unwrap();
    let replacement = invitation(loser, &joiner, gid, &new_kp, &added);
    let mut forged = added.history.clone();
    forged[1].rank = u8::MAX;
    let forged = postcard::to_allocvec(&forged).unwrap();
    let forged =
        proof::seal(&winner.provider, &winner.group(&gid), proof::INVITATION_LABEL, &forged)
            .unwrap();
    let old_branch = joiner.group(&gid).branch_id();
    assert!(accept(&replacement, &forged).is_err());
    assert_eq!(joiner.group(&gid).branch_id(), old_branch, "invalid history cannot replace keys");
    let accepted = accept(&replacement, &added.encrypted_history).unwrap();
    assert_eq!(accepted.branch_id(), winner.group(&gid).branch_id(), "the fresh KP survived");
    let prejoin = journal::Transaction::open(&joiner.provider, gid, Some(winning.parent)).unwrap();
    assert!(prejoin.is_none(), "the joiner never holds pre-join secrets");
    assert!(accept(&old_invite, &lost_add.encrypted_history).is_err(), "no rollback");
}

/// After the seven-day prune a refresh re-adds the same identity, only under the right anchor, and
/// replaying the request's nonce publishes no new epoch.
#[test]
fn refresh_after_secret_retirement_preserves_identity_and_rejects_repeated_nonce() {
    let (gid, a, b, c) = founded();
    let request = request(&c, &gid, GroupMemberAction::Refresh);
    let anchor =
        journal::history(&a.provider, &gid, a.group(&gid).branch_id()).unwrap()[0].branch.0;
    let old = c.group(&gid).branch_id();
    let quiet = GroupRules { members_send: false, ..GroupRules::default() };
    let rules = commit(&a, gid, Some(GroupChange::Rules(quiet)), vec![]).unwrap();
    receive(&b, gid, &rules).unwrap();
    let in_eight_days = common::utils::now_ms() / 1000 + 8 * 86400;
    journal::prune(&a.provider, &gid, in_eight_days).unwrap();
    assert!(journal::Transaction::open(&a.provider, gid, Some(old)).unwrap().is_none());
    let kp = c.kp();
    let refresh = GroupChange::MemberRequest(request.clone());
    let refreshed = commit(&a, gid, Some(refresh), vec![kp.clone()]).unwrap();
    let env = invitation(&a, &c, gid, &kp, &refreshed);
    let accept = |anchor| {
        journal::accept_welcome(
            &c.provider,
            &env,
            &refreshed.encrypted_history,
            Some(&request),
            anchor,
        )
    };
    assert!(accept(Some([9; 32])).is_err(), "another anchor is another group");
    let group = accept(Some(anchor)).unwrap();
    assert_eq!(group.branch_id(), a.group(&gid).branch_id());
    assert!(!group.group_meta().unwrap().effective().rules.members_send);
    let epoch = a.group(&gid).epoch();
    assert!(commit(&a, gid, Some(GroupChange::MemberRequest(request)), vec![c.kp()]).is_err());
    assert_eq!(a.group(&gid).epoch(), epoch, "a replayed nonce publishes no new epoch");
}

/// A signed leave carried after a takeover still applies, the departed member can still follow
/// the branch, an owner remains, and the request cannot be re-attributed.
#[test]
fn signed_departure_survives_epoch_changes_and_preserves_a_remaining_owner() {
    let (gid, a, b, c) = founded();
    let leave = request(&a, &gid, GroupMemberAction::Leave);
    let takeover = commit(&b, gid, Some(GroupChange::Takeover), vec![]).unwrap();
    receive(&c, gid, &takeover).unwrap();
    receive(&a, gid, &takeover).unwrap();
    let departed =
        commit(&b, gid, Some(GroupChange::MemberRequest(leave.clone())), vec![]).unwrap();
    receive(&c, gid, &departed).unwrap();
    // The branch id comes from the public tree and extensions, which reach the removed member too.
    receive(&a, gid, &departed).unwrap();
    for r in [&a, &b, &c] {
        assert_eq!(r.group(&gid).branch_id(), b.group(&gid).branch_id());
    }
    assert_eq!(b.group(&gid).group_meta().unwrap().effective().owners, vec![b.ipk]);
    assert!(!b.group(&gid).roster().contains(&a.ipk));
    let mut forged = leave;
    forged.who = c.ipk.into();
    assert!(proof::verify_member_request(&gid, &forged).is_err());
}

/// Another admin refreshes a committer that lost every key back in, ownership intact.
#[test]
fn another_admin_can_restore_the_committer_after_it_loses_all_epoch_keys() {
    let (gid, a, b, c) = founded();
    let request = request(&a, &gid, GroupMemberAction::Refresh);
    let anchor =
        journal::history(&b.provider, &gid, b.group(&gid).branch_id()).unwrap()[0].branch.0;
    let kp = a.kp();
    let refresh = GroupChange::MemberRequest(request.clone());
    let recovered = commit(&b, gid, Some(refresh), vec![kp.clone()]).unwrap();
    receive(&c, gid, &recovered).unwrap();
    a.provider.storage().forget_group(&gid).unwrap();
    let env = invitation(&b, &a, gid, &kp, &recovered);
    let group = journal::accept_welcome(
        &a.provider,
        &env,
        &recovered.encrypted_history,
        Some(&request),
        Some(anchor),
    )
    .unwrap();
    assert_eq!(group.branch_id(), b.group(&gid).branch_id());
    let state = group.group_meta().unwrap().effective();
    assert_eq!((state.owners, state.committer), (vec![a.ipk], b.ipk));
}

/// Live ingress refuses a substituted proof without consuming the genuine commit, which then
/// applies, and a correctly signed envelope in the retired format cannot bypass the journal.
#[tokio::test]
async fn live_ingress_rejects_substituted_proof_without_consuming_the_valid_commit() {
    use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
    use common::proto::mls_wire::MlsApplicationEnvelopeP;
    use common::proto::mls_wire::MlsEnvelopeP;
    use common::proto::mls_wire::group_envelope_signing_input;
    use common::proto::pack::Unpacker;

    use crate::data::conversation::Conversation;
    use crate::messaging::receive::InboundDecoded;
    use crate::messaging::send::SealedMessage;
    use crate::messaging::session::MlsContext;

    let scope = crate::test_support::ScopedCore::new();
    let (gid, a, b, c) = founded();
    let conversation = [211; 16];
    let sql = "INSERT INTO conversations(id,kind,created_by,mls_group_id) VALUES(?1,1,?2,?3)";
    let row = rusqlite::params![conversation, a.ipk, gid];
    scope.core.db.messages().lock().execute(sql, row).unwrap();
    let joined = c.group(&gid);
    Conversation::sync_group(&conversation, &joined.roster(), joined.group_meta().as_ref()).unwrap();
    let before = a.group(&gid);
    let rules = GroupRules { members_send: false, ..GroupRules::default() };
    let change = commit(&a, gid, Some(GroupChange::Rules(rules)), vec![]).unwrap();
    let transcript = group_envelope_signing_input(
        common::PROTOCOL_VERSION,
        &c.ipk,
        &gid,
        before.epoch(),
        &change.parent,
        &change.bytes,
    );
    let envelope = MlsApplicationEnvelopeP {
        version:     MLS_ENVELOPE_VERSION,
        group_id:    gid.into(),
        epoch:       before.epoch(),
        mls_message: change.bytes.clone().into(),
        sender_sig:  a.identity.sign(&transcript).to_bytes().into(),
    };
    let seal = |claimed: &GroupBranch| {
        let parent = journal::Transaction::open(&a.provider, gid, Some(change.parent)).unwrap();
        let parent = parent.unwrap();
        let bytes = postcard::to_allocvec(&(None::<GroupBranch>, claimed)).unwrap();
        proof::seal(&parent.provider, &parent.group, proof::COMMIT_LABEL, &bytes).unwrap()
    };
    let receive = |sealed| {
        let envelope = envelope.clone();
        crate::groups::recovery::receive(
            &c.provider, &c.ipk, a.ipk, change.parent, envelope, Some(sealed), [1; 16], 1000,
        )
    };
    let mut forged = change.proof.clone();
    forged.author = b.ipk.into();
    assert!(receive(seal(&forged)).is_err());
    assert_eq!(c.group(&gid).branch_id(), before.branch_id());
    receive(seal(&change.proof)).unwrap();
    assert_eq!(c.group(&gid).branch_id(), a.group(&gid).branch_id());
    assert!(!Conversation::state(&conversation).unwrap().rules.members_send);

    let stash = super::KeyPackageStash::new(c.db.clone());
    let buffer = super::EpochCatchupBuffer::new(c.db.clone());
    let dht = crate::test_support::net::FakeDhtClient::default();
    let ctx = MlsContext { provider: &c.provider, stash: &stash, buffer: &buffer, dht: &dht };
    let epoch = a.group(&gid).epoch();
    let old = SealedMessage { group_id: gid, epoch, mls_bytes: vec![], branch: None, proof: None };
    let Ok(MlsEnvelopeP::Application(old)) =
        MlsEnvelopeP::deser(&old.address_to(&c.ipk, &a.identity).unwrap())
    else {
        panic!("not an application envelope")
    };
    let outcome =
        crate::messaging::receive::process_application_inbound_for(&ctx, a.ipk, &c.ipk, old, 1000, [2; 16]);
    assert!(matches!(outcome.unwrap(), InboundDecoded::ApplicationStale));
}
