//! Reconnect orchestration and conversation projection for legacy migration.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use common::proto::mls_wire::GroupMigrationApproval;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::pack::Packer;
use rusqlite::params;

use crate::data::conversation::Conversation;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::mls::migration;

static RETRIES: once_cell::sync::Lazy<parking_lot::Mutex<std::collections::HashSet<[u8; 16]>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

fn retry_later(conversation: [u8; 16]) {
    if !RETRIES.lock().insert(conversation) {
        return;
    }
    crate::RUNTIME.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        RETRIES.lock().remove(&conversation);
        if crate::state::RELAY.read().is_some() {
            super::resume(conversation);
        }
    });
}

#[derive(Debug, thiserror::Error)]
#[error("Waiting for group members to update before upgrading this chat")]
pub(crate) struct Pending;

pub fn retired(provider: &PromtuzMlsProvider, group: &[u8; 32]) -> Result<bool> {
    Ok(migration::completed(provider, group)?.is_some()
        || crate::data::app_prefs::get(&format!("group_migrated:{}", hex::encode(group))).is_some())
}

/// This mapping is recorded beside the keys, before either network delivery
/// or the messages database changes. Repeating projection closes that gap.
pub fn finish(provider: &PromtuzMlsProvider, old: [u8; 32]) -> Result<()> {
    let Some((target, conversation)) = migration::completed(provider, &old)? else { return Ok(()) };
    let Some(row) = Conversation::get(&conversation) else { return Ok(()) };
    if row.mls_group_id.as_deref() == Some(target.as_slice()) {
        return Ok(());
    }
    if row.mls_group_id.as_deref() != Some(old.as_slice())
        && row.mls_group_id.as_deref() != Some(target.as_slice())
    {
        return Ok(());
    }
    if super::is_leaving(&conversation) || super::delete_pending(&conversation) {
        return Ok(());
    }
    let group = MlsGroupHandle::load(provider, &target)?
        .ok_or_else(|| anyhow!("migration target missing"))?;
    let history = crate::mls::recovery::history(provider, &target, group.branch_id())?;
    Conversation::migrate_group(
        &conversation,
        &old,
        &target,
        &group.roster(),
        &group.group_meta().ok_or_else(|| anyhow!("migration target is not a group"))?,
        history[0].branch.0,
    )?;
    if row.mls_group_id.as_deref() == Some(old.as_slice())
        && let Some(client) = crate::state::RELAY.read().as_ref().and_then(|r| r.dht_client.clone())
    {
        crate::RUNTIME.spawn(crate::quic::server::retry_pending_sends_once(client));
    }
    Ok(())
}

pub fn destination(provider: &PromtuzMlsProvider, target: &[u8; 32]) -> Result<Option<[u8; 16]>> {
    let Some(conversation) = migration::destination(provider, target)? else { return Ok(None) };
    let connection = provider.storage().connection();
    let old = connection.lock().query_row(
        "SELECT group_id FROM mls_group_migrations WHERE target=?1",
        [target],
        |r| r.get::<_, [u8; 32]>(0),
    )?;
    finish(provider, old)?;
    ensure!(
        Conversation::group_of(&conversation) == Some(*target),
        "migrated conversation was deleted or replaced"
    );
    Ok(Some(conversation))
}

pub fn received(old: [u8; 32], branch: [u8; 32], approval: GroupMigrationApproval) -> Result<()> {
    let provider = PromtuzMlsProvider::shared();
    if migration::completed(&provider, &old)?.is_some() {
        return finish(&provider, old);
    }
    let Some(conversation) = Conversation::for_group(&old) else { return Ok(()) };
    if super::is_leaving(&conversation) || super::delete_pending(&conversation) {
        return Ok(());
    }
    let Some(group) = MlsGroupHandle::load(&provider, &old)? else { return Ok(()) };
    let Some(source) = migration::Source::read(&group)? else { return Ok(()) };
    if source.branch != branch {
        return Ok(());
    }
    migration::remember(&provider, &source, &approval)?;
    super::resume(conversation);
    Ok(())
}

pub fn accept(
    old: [u8; 32], branch: [u8; 32], from: [u8; 32], approvals: &[GroupMigrationApproval],
    welcome: &WelcomeEnvelopeP, history: &[u8], signature: &[u8; 64],
) -> Result<()> {
    let provider = PromtuzMlsProvider::shared();
    let conversation = migration::completed(&provider, &old)?
        .map(|(_, c)| c)
        .or_else(|| Conversation::for_group(&old));
    let Some(conversation) = conversation else { return Ok(()) };
    if Conversation::get(&conversation).is_none()
        || super::is_leaving(&conversation)
        || super::delete_pending(&conversation)
    {
        return Ok(());
    }
    migration::accept(
        &provider,
        old,
        branch,
        conversation,
        super::local_ipk()?,
        from,
        approvals,
        welcome,
        history,
        signature,
    )?;
    finish(&provider, old)?;
    super::resume(conversation);
    Ok(())
}

/// Returns true when legacy migration owns this group's next step.
pub async fn follow_up(conversation: [u8; 16]) -> Result<bool> {
    let provider = PromtuzMlsProvider::shared();
    let Some(old) = Conversation::group_of(&conversation) else { return Ok(false) };
    if migration::completed(&provider, &old)?.is_some() {
        finish(&provider, old)?;
        return Ok(false);
    }
    let Some(group) = MlsGroupHandle::load(&provider, &old)? else { return Ok(false) };
    let Some(source) = migration::Source::read(&group)? else { return Ok(false) };
    if super::is_leaving(&conversation) || super::delete_pending(&conversation) {
        return Ok(true);
    }
    retry_later(conversation);
    let (me, signer) = super::local_signer()?;
    let approval = source.approve(&group, &signer)?;
    migration::remember(&provider, &source, &approval)?;
    if source.founder != me {
        let now = crate::utils::systime().as_secs();
        let last: u64 = provider.storage().connection().lock().query_row(
            "SELECT last_sent FROM mls_migration_consents WHERE group_id=?1 AND who=?2",
            params![old, me],
            |r| r.get(0),
        )?;
        if now.saturating_sub(last) < 60 {
            return Ok(true);
        }
        // One durable approval per member and source branch, even while the
        // founder is offline. Reconnect must not accumulate duplicate controls.
        let id = crate::mls::recovery::dispatch_id(
            &source.branch,
            &me[..16].try_into().expect("identity prefix"),
        );
        let payload = MlsEnvelopeP::GroupMigrationReady {
            group: old.into(),
            branch: source.branch.into(),
            approval,
        }
        .ser()?;
        let frame = crate::messaging::prepare_dispatch(
            &source.founder,
            &me,
            &signer,
            &id,
            payload,
            common::proto::client_rel::Wake::Message,
            0,
        )?;
        let mut copies = vec![(source.founder, id, crate::db::outbox::OpType::Control, frame)];
        crate::delivery::enqueue_batch(&mut copies)?;
        provider.storage().connection().lock().execute(
            "UPDATE mls_migration_consents SET last_sent=?3 WHERE group_id=?1 AND who=?2 AND branch=?4",
            params![old,me,now,source.branch])?;
        super::recovery::dispatch(copies).await;
        return Ok(true);
    }
    let approvals = migration::approvals(&provider, &source)?;
    if approvals.len() != source.members.len() {
        return Ok(true);
    }
    source.verify_all(&approvals)?;
    let _one = super::MEMBERSHIP.lock().await;
    let client = crate::state::RELAY.read().as_ref().and_then(|r| r.dht_client.clone());
    let Some(client) = client else { return Ok(true) };
    let stash = crate::mls::KeyPackageStash::new(provider.storage().connection());
    let buffer = crate::mls::EpochCatchupBuffer::new(provider.storage().connection());
    let ctx = crate::messaging::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    let mut packages = Vec::new();
    for who in source.members.iter().filter(|m| **m != me) {
        let (kp, reference) = crate::messaging::fetch_verified_keypackage(&ctx, who, true).await?;
        packages.push((*who, kp, reference));
    }
    // Re-read in the isolated publication operation after the network await.
    let target = migration::create(&provider, old, conversation, &signer, &approvals, &packages)?;
    finish(&provider, old)?;
    super::recovery::dispatch(super::recovery::flush_jobs(&provider, target)?).await;
    Ok(false)
}
