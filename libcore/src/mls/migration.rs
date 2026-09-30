//! Unanimous, identity-authenticated migration of pre-binding group chats.
//! An unsigned legacy leaf is never accepted as a message author. Instead,
//! every named identity consents to the exact old tree and context, and the
//! founder establishes fresh bound keys for precisely that roster.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use common::proto::mls_wire::GroupMigrationApproval;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::group_migration_signing_input;
use common::proto::mls_wire::group_migration_welcome_signing_input;
use common::proto::pack::Packer;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use openmls::prelude::KeyPackage;
use openmls_traits::OpenMlsProvider;
use rusqlite::OptionalExtension;
use rusqlite::params;
use sha2::Digest;
use sha2::Sha256;

use super::GroupMeta;
use super::MlsGroupHandle;
use super::PromtuzMlsProvider;
use super::branch_proof;
use super::credential;
use super::recovery;

pub struct Source {
    pub group:   [u8; 32],
    pub branch:  [u8; 32],
    pub founder: [u8; 32],
    pub members: Vec<[u8; 32]>,
    pub title:   String,
}

impl Source {
    pub fn read(group: &MlsGroupHandle) -> Result<Option<Self>> {
        let Some(meta) = group.group_meta().filter(|m| m.state.is_none()) else { return Ok(None) };
        let mut members = Vec::new();
        let mut legacy = false;
        for m in group.members() {
            let id = credential::leaf_identity(&m.credential, &m.signature_key)
                .ok_or_else(|| anyhow!("legacy group has an invalid identity credential"))?;
            legacy |= matches!(id, credential::LeafIdentity::Legacy(_));
            members.push(id.ipk());
        }
        if !legacy {
            return Ok(None);
        }
        members.sort();
        ensure!(
            members.len() <= super::MAX_GROUP_MEMBERS
                && members.windows(2).all(|w| w[0] != w[1])
                && members.contains(&meta.founder),
            "invalid legacy roster"
        );
        Ok(Some(Self {
            group: group.group_id(),
            branch: group.branch_id(),
            founder: meta.founder,
            members,
            title: meta.title,
        }))
    }

    pub fn target(&self) -> [u8; 32] {
        Sha256::digest(group_migration_signing_input(&self.group, &self.branch)).into()
    }

    pub fn approve(
        &self, group: &MlsGroupHandle, signer: &SigningKey,
    ) -> Result<GroupMigrationApproval> {
        let who = signer.verifying_key().to_bytes();
        ensure!(
            group.group_id() == self.group
                && group.branch_id() == self.branch
                && group.migration_identity() == Some(who),
            "migration identity is not our own seat"
        );
        Ok(GroupMigrationApproval {
            who:       who.into(),
            signature: signer
                .sign(&group_migration_signing_input(&self.group, &self.branch))
                .to_bytes()
                .into(),
        })
    }

    pub fn verify(&self, approval: &GroupMigrationApproval) -> Result<()> {
        ensure!(self.members.contains(&approval.who.0), "migration approval is not from a member");
        VerifyingKey::from_bytes(&approval.who.0)?.verify_strict(
            &group_migration_signing_input(&self.group, &self.branch),
            &Signature::from_bytes(&approval.signature.0),
        )?;
        Ok(())
    }

    pub fn verify_all(&self, approvals: &[GroupMigrationApproval]) -> Result<()> {
        ensure!(approvals.len() == self.members.len(), "migration needs every member's approval");
        for (who, approval) in self.members.iter().zip(approvals) {
            ensure!(*who == approval.who.0, "migration approvals repeat or change members");
            self.verify(approval)?;
        }
        Ok(())
    }

    fn verify_target(&self, group: &MlsGroupHandle) -> Result<()> {
        let mut roster = group.roster();
        roster.sort();
        ensure!(
            group.group_id() == self.target()
                && roster == self.members
                && group.group_meta() == Some(GroupMeta::founded(self.title.clone(), self.founder)),
            "migration changes the group's identity, members or rules"
        );
        Ok(())
    }
}

pub fn completed(
    provider: &PromtuzMlsProvider, old: &[u8; 32],
) -> Result<Option<([u8; 32], [u8; 16])>> {
    Ok(provider
        .storage()
        .connection()
        .lock()
        .query_row(
            "SELECT target,conversation FROM mls_group_migrations WHERE group_id=?1",
            [old],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn destination(provider: &PromtuzMlsProvider, target: &[u8; 32]) -> Result<Option<[u8; 16]>> {
    Ok(provider
        .storage()
        .connection()
        .lock()
        .query_row("SELECT conversation FROM mls_group_migrations WHERE target=?1", [target], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn remember(
    provider: &PromtuzMlsProvider, source: &Source, approval: &GroupMigrationApproval,
) -> Result<()> {
    source.verify(approval)?;
    provider.storage().connection().lock().execute(
        "INSERT INTO mls_migration_consents(group_id,branch,who,signature) VALUES(?1,?2,?3,?4)
         ON CONFLICT(group_id,who) DO UPDATE SET branch=excluded.branch,signature=excluded.signature,last_sent=0
         WHERE branch<>excluded.branch",
        params![source.group,source.branch,approval.who.0,approval.signature.0],
    )?;
    Ok(())
}

pub fn approvals(
    provider: &PromtuzMlsProvider, source: &Source,
) -> Result<Vec<GroupMigrationApproval>> {
    let connection = provider.storage().connection();
    let conn = connection.lock();
    let mut q = conn.prepare("SELECT who,signature FROM mls_migration_consents WHERE group_id=?1 AND branch=?2 ORDER BY who")?;
    Ok(q.query_map(params![source.group, source.branch], |r| {
        Ok(GroupMigrationApproval {
            who:       r.get::<_, [u8; 32]>(0)?.into(),
            signature: r.get::<_, [u8; 64]>(1)?.into(),
        })
    })?
    .collect::<rusqlite::Result<_>>()?)
}

pub fn create(
    provider: &PromtuzMlsProvider, old: [u8; 32], conversation: [u8; 16], signer: &SigningKey,
    approvals: &[GroupMigrationApproval], packages: &[([u8; 32], KeyPackage, [u8; 32])],
) -> Result<[u8; 32]> {
    let _guard = recovery::operation_lock(&old).lock();
    if let Some((target, _)) = completed(provider, &old)? {
        return Ok(target);
    }
    let operation = recovery::Replacement::open(provider, old)?;
    let original = MlsGroupHandle::load(&operation.provider, &old)?
        .ok_or_else(|| anyhow!("missing source"))?;
    let source =
        Source::read(&original)?.ok_or_else(|| anyhow!("group does not need migration"))?;
    source.verify_all(approvals)?;
    ensure!(
        source.founder == signer.verifying_key().to_bytes(),
        "only the founder migrates the group"
    );
    source.approve(&original, signer)?;
    let mut adding = Vec::new();
    for (who, kp, reference) in packages {
        ensure!(
            *who != source.founder
                && credential::leaf_node_ipk(kp.leaf_node(), true) == Some(*who)
                && kp.hash_ref(operation.provider.crypto())?.as_slice() == reference,
            "invalid migration KeyPackage"
        );
        adding.push(*who);
    }
    adding.sort();
    ensure!(
        adding
            == source.members.iter().copied().filter(|m| *m != source.founder).collect::<Vec<_>>(),
        "migration packages change the roster"
    );
    let gid = source.target();
    let (leaf, credential) = crate::messaging::build_self_credential(signer)?;
    leaf.store(operation.provider.storage()).map_err(|e| anyhow!("store signer: {e:?}"))?;
    let mut group = MlsGroupHandle::create(
        &operation.provider,
        &leaf,
        credential,
        &gid,
        Some(&GroupMeta::founded(source.title.clone(), source.founder)),
    )?;
    let welcome = if packages.is_empty() {
        None
    } else {
        let (_, welcome) = group.add_members(
            &operation.provider,
            &leaf,
            &packages.iter().map(|(_, kp, _)| kp.clone()).collect::<Vec<_>>(),
        )?;
        group.merge_pending_commit(&operation.provider)?;
        Some(welcome)
    };
    source.verify_target(&group)?;
    let history = vec![branch_proof::sign(&group, None, 0, &[], signer)?];
    let sealed = branch_proof::seal(
        &operation.provider,
        &group,
        branch_proof::INVITATION_LABEL,
        &postcard::to_allocvec(&history)?,
    )?;
    let mut jobs = Vec::new();
    for (who, _, reference) in packages {
        let welcome = super::make_welcome_envelope(
            welcome.clone().expect("members have a Welcome"),
            gid,
            source.founder,
            *who,
            *reference,
            signer,
        )?;
        let signature = signer.sign(&group_migration_welcome_signing_input(
            &old,
            &source.branch,
            approvals,
            &welcome,
            &sealed,
        ));
        let envelope = MlsEnvelopeP::GroupMigrationWelcome {
            group: old.into(),
            branch: source.branch.into(),
            approvals: approvals.to_vec(),
            welcome,
            history: sealed.clone().into(),
            signature: signature.to_bytes().into(),
        };
        let id = ulid::Ulid::new().to_bytes();
        jobs.push(recovery::DispatchJob {
            recipient: *who,
            id,
            logical_id: id,
            kind: crate::db::outbox::OpType::Welcome as i64,
            frame: crate::messaging::prepare_dispatch(
                who,
                &source.founder,
                signer,
                &id,
                envelope.ser()?,
                common::proto::client_rel::Wake::Message,
                0,
            )?,
        });
    }
    operation.publish(&group, &history, conversation, &jobs, None)?;
    Ok(gid)
}

pub fn accept(
    provider: &PromtuzMlsProvider, old: [u8; 32], branch: [u8; 32], conversation: [u8; 16],
    me: [u8; 32], from: [u8; 32], approvals: &[GroupMigrationApproval],
    envelope: &WelcomeEnvelopeP, sealed_history: &[u8], signature: &[u8; 64],
) -> Result<[u8; 32]> {
    ensure!(
        approvals.len() <= super::MAX_GROUP_MEMBERS
            && sealed_history.len() <= common::proto::mls_wire::MAX_WELCOME_BYTES
            && envelope.welcome_blob.0.len() <= common::proto::mls_wire::MAX_WELCOME_BYTES,
        "migration too large"
    );
    VerifyingKey::from_bytes(&from)?.verify_strict(
        &group_migration_welcome_signing_input(&old, &branch, approvals, envelope, sealed_history),
        &Signature::from_bytes(signature),
    )?;
    ensure!(
        envelope.sender_ipk.0 == from && envelope.recipient_ipk.0 == me,
        "misaddressed migration"
    );
    let _guard = recovery::operation_lock(&old).lock();
    if let Some((target, saved_conversation)) = completed(provider, &old)? {
        ensure!(
            target == envelope.group_id.0 && saved_conversation == conversation,
            "conflicting migration"
        );
        return Ok(target);
    }
    let operation = recovery::Replacement::open(provider, old)?;
    let original = MlsGroupHandle::load(&operation.provider, &old)?
        .ok_or_else(|| anyhow!("missing source"))?;
    let source =
        Source::read(&original)?.ok_or_else(|| anyhow!("group does not need migration"))?;
    ensure!(
        source.branch == branch
            && source.founder == from
            && original.migration_identity() == Some(me),
        "migration does not match our current group"
    );
    source.verify_all(approvals)?;
    let group = super::process_welcome(&operation.provider, envelope)?;
    source.verify_target(&group)?;
    let history =
        postcard::from_bytes::<Vec<common::proto::mls_wire::GroupBranch>>(&branch_proof::open(
            &operation.provider,
            &group,
            branch_proof::INVITATION_LABEL,
            sealed_history,
        )?)?;
    operation.publish(&group, &history, conversation, &[], Some(envelope.kp_ref_used.0))?;
    Ok(source.target())
}
