//! Pairing and Welcomes: starting a pair, accepting a Welcome, and introducing ourselves.

use std::collections::HashMap;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::PairingP;
use common::proto::mls_wire::WelcomeEnvelopeP;
use log::debug;
use log::info;
use log::warn;

use super::send_control;
use super::send_control_to;
use super::session::MlsContext;
use super::session::lazy_create_group_paired;
use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::delivery::send_pair_decline;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::mls::process_welcome;
use crate::quic::dht_client::DhtClient;
use crate::state::core;

/// Proof of pair: being decryptable is what marks the inviter's contact paired.
pub async fn send_pair_ack(to: [u8; 32]) -> Result<()> {
    let conversation = Conversation::for_peer(&to)?;
    send_control(conversation, AppPayload::PairAck).await
}

/// `from` deleted our direct chat and its pair group. We drop our copy of the group but keep the
/// history; our next message starts a fresh pair, which reaches them as a request.
pub(crate) fn unpaired(conversation: [u8; 16], from: [u8; 32]) {
    let direct = Conversation::get(&conversation)
        .is_some_and(|c| c.kind == crate::data::conversation::KIND_DIRECT);
    let Some(gid) = Conversation::group_of(&conversation) else { return };
    if !direct || Conversation::peer_of(&conversation) != Some(from) {
        return;
    }
    match Contact::unpair(&from) {
        Ok(true) => {},
        Ok(false) => return,
        Err(e) => {
            return warn!("PAIR: could not drop the pair {} ended: {e}", hex::encode(&from[..4]));
        },
    }
    if let Err(e) = Conversation::unbind_group(&conversation) {
        warn!("PAIR: could not unbind the ended pair group: {e}");
    }
    crate::p2p::drop_link(&from);
    crate::api::messaging::purge_mls_group(&gid);
    info!("PAIR: {} deleted our chat; the next message starts a fresh pair", hex::encode(&from[..4]));
}

/// Confirm a working pair with `to`, then show them who we are. They hold our
/// name from an invite; a requester learns it here.
pub(crate) fn confirm_pair(to: [u8; 32]) {
    core().spawn(async move {
        if let Err(e) = send_pair_ack(to).await {
            warn!("PAIR: ack send to {} failed: {e}", hex::encode(&to[..4]));
            return;
        }
        if let Ok(conv) = Conversation::for_peer(&to) {
            introduce_ourselves(conv);
        }
    });
}

/// Builds the pair group and publishes a Welcome carrying our invite and name. The contact is saved
/// as pending only on success, so an unreachable peer leaves no dead row.
pub async fn pair(to: [u8; 32], peer_name: String, pairing: PairingP) -> Result<()> {
    pair_with_consent(to, peer_name, Some(pairing)).await
}

pub(crate) async fn pair_with_consent(
    to: [u8; 32], peer_name: String, pairing: Option<PairingP>,
) -> Result<()> {
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    let client = core()
        .session()
        .map(|s| s.dht.clone())
        .ok_or_else(|| anyhow!("not connected to a relay; reconnect before pairing"))?;
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(core().db.mls());
    let buffer = EpochCatchupBuffer::new(core().db.mls());
    let ctx = MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };

    let group = lazy_create_group_paired(&ctx, &our_ipk, &ipk_signer, &to, pairing).await?;
    Contact::save_pending(to, peer_name)?;
    if let Err(e) = Contact::set_mls_group_id(&to, &group.group_id()) {
        warn!("PAIR: persist mls_group_id failed: {e}");
    }
    Ok(())
}

/// Name, picture and profile details, each in its own control message so a member holding one
/// loses nothing when only another changes.
pub(super) fn own_introduction() -> Vec<AppPayload> {
    let Some(identity) = Identity::get() else { return Vec::new() };
    let mut out = Vec::with_capacity(2);
    let name = identity.name();
    if !name.is_empty() {
        out.push(AppPayload::Profile { name });
    }
    // Include removals too, so introductions cannot revive a stale picture.
    out.push(identity.avatar_update().into_payload());
    out.push(identity.details().into_payload());
    out
}

/// Spawned because every caller is on a receive path that must not block on a send.
pub(crate) fn introduce_ourselves(conversation: [u8; 16]) {
    let payloads = own_introduction();
    if payloads.is_empty() {
        return;
    }
    core().spawn(async move {
        for payload in payloads {
            if let Err(e) = send_control(conversation, payload).await {
                debug!("PROFILE: could not introduce ourselves: {e}");
            }
        }
    });
}

/// For a member who joined after us and never heard our introduction.
pub(crate) fn introduce_ourselves_to(conversation: [u8; 16], who: [u8; 32]) {
    let mut payloads = own_introduction();
    let me = Identity::local_ipk().unwrap_or_default();
    if Conversation::may_edit(&conversation, &me)
        && let Some((revision, avif)) = crate::data::group_picture::snapshot(&conversation)
    {
        payloads.push(AppPayload::GroupPicture { revision, avif });
    }
    if payloads.is_empty() {
        return;
    }
    core().spawn(async move {
        for payload in payloads {
            if let Err(e) = send_control_to(conversation, payload, who).await {
                debug!("PROFILE: could not introduce ourselves to a new member: {e}");
            }
        }
    });
}

/// A removal travels too, so members stop showing a picture we took down.
pub(crate) fn broadcast_avatar(update: crate::data::peer_avatar::AvatarUpdate) {
    broadcast_profile(update.into_payload());
}

pub(crate) fn broadcast_profile(payload: AppPayload) {
    let Some(me) = Identity::local_ipk() else { return };
    let chats: Vec<[u8; 16]> = Conversation::list()
        .into_iter()
        .filter(|c| c.mls_group_id.is_some())
        .map(|c| c.id)
        .filter(|id| Conversation::members(id).iter().any(|m| m.active && m.member_ipk == me))
        .filter(|id| !crate::requests::is_request_chat(id))
        .collect();
    core().spawn(async move {
        for id in chats {
            if let Err(e) = send_control(id, payload.clone()).await {
                debug!("PROFILE: could not send our picture to {}: {e}", hex::encode(&id[..4]));
            }
        }
    });
}

/// The group context, not the roster, says whether this is a group: a group of two and a pair
/// have the same roster. A group chat always gets its own conversation, never a DM's.
pub(crate) fn home_for_group(group: &MlsGroupHandle, from: &[u8; 32]) -> Result<[u8; 16]> {
    let gid = group.group_id();
    ensure!(!crate::groups::migration::retired(&PromtuzMlsProvider::shared(), &gid)?, "group session was migrated");
    if let Some(id) = crate::groups::migration::destination(&PromtuzMlsProvider::shared(), &gid)? {
        return Ok(id);
    }
    if let Some(id) = Conversation::for_group(&gid) {
        return Ok(id); // already homed; a redelivered Welcome mints no second one
    }
    let Some(meta) = group.group_meta() else {
        ensure!(!group.is_group_chat(), "cannot open unsupported group rules as a direct chat");
        let id = Conversation::for_peer(from)?;
        Conversation::bind_group(&id, &gid)?;
        let _ = Contact::set_mls_group_id(from, &gid);
        return Ok(id);
    };
    let roster = group.roster();
    // Roles from the context, not from `from`: on a group re-opened by an
    // arriving message, `from` is whoever spoke first, not who runs the group.
    let id = Conversation::join_group(&meta.founder, &roster)?;
    Conversation::bind_group(&id, &gid)?;
    Conversation::sync_group(&id, &roster, Some(&meta))?;
    if !meta.title.is_empty() {
        let _ = Conversation::set_title(&id, &meta.title);
    }
    introduce_ourselves(id);
    // Whatever the committer sent us before our Welcome landed was dropped:
    // its introduction and the group's photo. Ask again.
    let committer = meta.effective().committer;
    if Identity::get().is_some_and(|i| i.ipk() != committer) {
        core().spawn(async move {
            let ask = AppPayload::GroupRequest(common::proto::mls_wire::GroupRequest::Sync);
            if let Err(e) = send_control_to(id, ask, committer).await {
                debug!("GROUP: could not ask the committer to catch us up: {e}");
            }
        });
    }
    crate::groups::resume(id);
    info!(
        "GROUP: opened \"{}\" ({} members) as conversation {}",
        meta.title,
        roster.len(),
        hex::encode(&id[..4])
    );
    Ok(id)
}

pub(super) fn valid_initial_pair(
    roster: &[[u8; 32]], has_group_meta: bool, ours: &[u8; 32], peer: &[u8; 32],
) -> bool {
    !has_group_meta
        && roster.len() == 2
        && ours != peer
        && roster.contains(ours)
        && roster.contains(peer)
}

/// Who a Welcome lets in, by the sender's standing with us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// A contact, or a group we are recovering: no new contact and no request.
    Known,
    /// A stranger with an invite we minted becomes a contact.
    Invited,
    /// A stranger, or a requester still waiting, opens a message request.
    Request,
    Refused,
}

/// `status` is the sender's contact status. `requests_allowed` is asked only about a stranger
/// without an invite.
fn admission(
    recovering_group: bool, status: Option<u8>, invited: bool,
    requests_allowed: impl FnOnce() -> bool,
) -> Admission {
    if recovering_group || status.is_some_and(|s| s != crate::data::contact::PAIR_STATUS_REQUEST) {
        Admission::Known
    } else if invited {
        Admission::Invited
    } else if status.is_some() || requests_allowed() {
        Admission::Request
    } else {
        Admission::Refused
    }
}

/// Whether the Welcome was accepted; the caller acks either way, so queued junk cannot block inbox
/// sync. A refusal after the gates is answered with a `PairDecline`; an accepted pair is confirmed.
pub(super) fn process_welcome_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: WelcomeEnvelopeP, history: Option<&[u8]>,
) -> Result<bool> {
    if crate::groups::migration::retired(ctx.provider, &env.group_id.0)? {
        return Ok(false);
    }
    if env.sender_ipk.0 != sender_ipk {
        warn!("MLS: dropped Welcome with sender_ipk mismatch with DispatchP.from");
        return Ok(false);
    }

    // The signature already binds `recipient_ipk`, but a delivery bug could still hand us another
    // device's envelope: drop and ack it rather than retry forever.
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    if env.recipient_ipk.0 != our_ipk {
        warn!(
            "MLS: dropped Welcome addressed to {} (we are {})",
            hex::encode(&env.recipient_ipk.0[..4]),
            hex::encode(&our_ipk[..4])
        );
        return Ok(false);
    }

    // A stranger's Welcome needs an invite we minted, or opens a message request when allowed. The
    // name is saved only after a successful accept, so a failed one leaves no contact behind.
    if crate::requests::is_blocked(&sender_ipk) {
        return Ok(false);
    }
    if crate::groups::member_requests::left(&env.group_id.0) {
        return Ok(false);
    }
    let recovering_group = history.is_some() && Conversation::for_group(&env.group_id.0).is_some();
    let status = Contact::status(&sender_ipk);
    let invite = env.pairing.as_ref().filter(|p| Identity::verify_invite(&p.invite));
    let admitted =
        admission(recovering_group, status, invite.is_some(), crate::requests::admits_stranger);
    let (new_contact_name, redeemed_invite, request) = match (admitted, invite) {
        (Admission::Known, _) => (None, None, false),
        (Admission::Invited, Some(pairing)) => (
            Some(pairing.sender_name.chars().take(32).collect::<String>()),
            Some(pairing.invite.clone()),
            false,
        ),
        (Admission::Request, _) => (None, None, true),
        _ => {
            warn!(
                "MLS: dropped Welcome from unknown sender {} (no valid invite)",
                hex::encode(&sender_ipk[..4])
            );
            return Ok(false);
        },
    };
    // A request is turned down silently, so its sender learns nothing.
    let refuse = || -> Result<bool> {
        if !request {
            core().spawn(async move {
                let reason = common::proto::mls_wire::DECLINE_GROUP_BUILD_FAILED;
                if let Err(e) = send_pair_decline(sender_ipk, reason).await {
                    warn!("PAIR: decline send failed: {e}");
                }
            });
        }
        Ok(false)
    };

    // After the gate, an accept failure is a decline rather than an error: we were invited but
    // could not build the group.
    if history.is_none() && MlsGroupHandle::load(ctx.provider, &env.group_id.0)?.is_some() {
        return Ok(false);
    }
    let accepted = if let Some(history) = history {
        crate::mls::recovery::accept_welcome(
            ctx.provider,
            &env,
            history,
            crate::groups::member_requests::refresh_request(&env.group_id.0).as_ref(),
            crate::groups::member_requests::known_anchor(&env.group_id.0),
        )
    } else {
        process_welcome_inbound_no_contacts(ctx, sender_ipk, env)
    };
    let mut group = match accepted {
        Ok(g) => g,
        Err(e) => {
            warn!("MLS: welcome accept failed from {}: {e}", hex::encode(&sender_ipk[..4]));
            return refuse();
        },
    };

    // A stranger may only open a direct chat, or a signed Welcome could pull a multi-person group
    // into a conversation the user believed was private.
    if (new_contact_name.is_some() || request)
        && !valid_initial_pair(&group.roster(), group.group_meta().is_some(), &our_ipk, &sender_ipk)
    {
        let _ = group.delete(ctx.provider);
        warn!("MLS: initial pairing Welcome was not a two-person direct chat");
        return refuse();
    }

    if history.is_some() {
        let path =
            crate::mls::recovery::history(ctx.provider, &group.group_id(), group.branch_id())?;
        crate::groups::member_requests::anchor(&group.group_id(), path[0].branch.0)?;
        crate::groups::member_requests::refreshed(&group.group_id())?;
    }
    // Accepted: save the contact, which defaults to paired.
    if let Some(name) = new_contact_name {
        match Contact::save(sender_ipk, name) {
            Ok(_) => info!("IDENTITY: paired with {}", hex::encode(&sender_ipk[..4])),
            Err(e) => warn!("IDENTITY: failed to save paired contact: {e}"),
        }
        if let Some(invite) = redeemed_invite {
            Identity::spend_invite(&invite);
        }
    }
    // Without its row the chat would open as an ordinary one, ungated.
    if request && status.is_none() {
        if let Err(e) = Contact::save_request(sender_ipk) {
            let _ = group.delete(ctx.provider);
            warn!("REQUEST: could not record a request from {}: {e}", hex::encode(&sender_ipk[..4]));
            return Ok(false);
        }
        info!("REQUEST: message request from {}", hex::encode(&sender_ipk[..4]));
    }
    if let Some(conversation) = Conversation::for_group(&group.group_id()) {
        Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
        crate::groups::resume(conversation);
    }
    if let Err(e) = home_for_group(&group, &sender_ipk) {
        // The MLS state is sound; we just have nowhere to show it. Say so
        // loudly rather than silently filing a group under someone's DM.
        warn!("MLS: welcomed into a group we could not open a chat for: {e}");
    }

    info!(
        "MLS: welcome from {} activated group {}",
        hex::encode(&sender_ipk[..4]),
        hex::encode(&group.group_id()[..4])
    );
    // A request waits for the user to accept it.
    if !crate::requests::is_request(&sender_ipk) {
        confirm_pair(sender_ipk);
    }
    Ok(true)
}

/// Builds the group and marks the KeyPackage consumed without touching contacts. The caller must
/// already have gated the sender.
pub fn process_welcome_inbound_no_contacts<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: WelcomeEnvelopeP,
) -> Result<MlsGroupHandle> {
    if env.sender_ipk.0 != sender_ipk {
        bail!("welcome envelope sender_ipk mismatch with DispatchP.from");
    }
    let kp_ref = env.kp_ref_used.0;
    let group = process_welcome(ctx.provider, &env).map_err(|e| anyhow!("process_welcome: {e}"))?;
    if let Err(e) = ctx.stash.on_consumed(&kp_ref) {
        warn!("MLS: stash on_consumed failed: {e}");
    }
    Ok(group)
}

/// After this many failed polls, a Welcome from an unknown sender is acked and dropped.
const POLL_WELCOMES_MAX_RETRY: u8 = 5;

/// Per-`welcome_id` failure counts, in memory only: a restart resets them, and the home's TTL
/// still bounds the queue.
static WELCOME_RETRY_COUNTS: std::sync::LazyLock<parking_lot::Mutex<HashMap<[u8; 8], u8>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Returns how many Welcomes were accepted. Every outcome is acked except an error from an
/// unknown sender, held for `POLL_WELCOMES_MAX_RETRY` polls.
pub async fn poll_welcomes<C: DhtClient>(ctx: &MlsContext<'_, C>) -> Result<usize> {
    let entries = ctx.dht.fetch_welcomes().await.map_err(|e| anyhow!("fetch_welcomes: {e}"))?;

    let mut ack_ids: Vec<[u8; 8]> = Vec::with_capacity(entries.len());
    let mut count = 0usize;
    for entry in entries {
        // A queued Welcome has no verified dispatch around it: the envelope's own `sender_ipk`,
        // checked by `process_welcome`'s signature verification, is the authentication.
        let sender_ipk = entry.envelope.sender_ipk.0;
        let welcome_id = entry.welcome_id.0;
        let known_contact = Contact::exists(&sender_ipk);
        match process_welcome_inbound(ctx, sender_ipk, entry.envelope, None) {
            Ok(accepted) => {
                ack_ids.push(welcome_id);
                count += usize::from(accepted);
                WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
            },
            Err(e) => {
                if known_contact {
                    // Re-fetching the same bytes from a known contact will not help, so ack.
                    log::warn!(
                        "MLS: poll_welcomes: drop bad welcome from known contact {}: {e}",
                        hex::encode(&sender_ipk[..4])
                    );
                    ack_ids.push(welcome_id);
                    WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
                } else {
                    // Hold for the next reconnect: the sender may become a contact meanwhile.
                    let mut counts = WELCOME_RETRY_COUNTS.lock();
                    let entry_count = counts.entry(welcome_id).or_insert(0);
                    *entry_count = entry_count.saturating_add(1);
                    let attempts = *entry_count;
                    drop(counts);
                    log::warn!(
                        "MLS: poll_welcomes: hold welcome from unknown {} (attempt {}/{}): {e}",
                        hex::encode(&sender_ipk[..4]),
                        attempts,
                        POLL_WELCOMES_MAX_RETRY
                    );
                    if attempts >= POLL_WELCOMES_MAX_RETRY {
                        log::warn!(
                            "MLS: poll_welcomes: ack-and-drop welcome from unknown {} after {} attempts",
                            hex::encode(&sender_ipk[..4]),
                            attempts
                        );
                        ack_ids.push(welcome_id);
                        WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
                    }
                }
            },
        }
    }

    if !ack_ids.is_empty()
        && let Err(e) = ctx.dht.ack_welcomes(&ack_ids).await
    {
        log::warn!("MLS: poll_welcomes: ack_welcomes failed: {e}");
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::react;
    use crate::messaging::session::build_self_credential;
    use crate::messaging::session::fetch_verified_keypackage;
    use crate::mls::GroupMeta;
    use crate::mls::make_welcome_envelope;
    use crate::test_support::ScopedCore;
    use crate::test_support::data::identity;
    use crate::test_support::net::Device;
    use crate::test_support::net::FakeDhtClient;

    /// Who a Welcome lets in. Only a stranger without our invite depends on the requests setting.
    #[test]
    fn welcome_admission_follows_the_senders_standing() {
        use Admission::*;

        use crate::data::contact::PAIR_STATUS_PAIRED as PAIRED;
        use crate::data::contact::PAIR_STATUS_PENDING as PENDING;
        use crate::data::contact::PAIR_STATUS_REQUEST as REQUEST;
        // (recovering a group, contact status, invited, requests on) -> (admission, setting read)
        let rows = [
            ((true, None, false, false), (Known, false)),
            ((false, Some(PAIRED), false, false), (Known, false)),
            ((false, Some(PENDING), false, false), (Known, false)),
            ((false, Some(REQUEST), false, false), (Request, false)),
            ((false, Some(REQUEST), true, false), (Invited, false)),
            ((false, None, true, false), (Invited, false)),
            ((false, None, false, true), (Request, true)),
            ((false, None, false, false), (Refused, true)),
        ];
        for ((recovering, status, invited, requests_on), want) in rows {
            let asked = std::cell::Cell::new(false);
            let got = admission(recovering, status, invited, || {
                asked.set(true);
                requests_on
            });
            assert_eq!((got, asked.get()), want, "{recovering} {status:?} {invited} {requests_on}");
        }
    }

    #[test]
    fn discovery_consent_cannot_be_used_to_join_a_room_or_a_different_peer() {
        let (me, peer, third) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        assert!(valid_initial_pair(&[peer, me], false, &me, &peer));
        assert!(!valid_initial_pair(&[peer, me], true, &me, &peer));
        assert!(!valid_initial_pair(&[peer, me, third], false, &me, &peer));
        assert!(!valid_initial_pair(&[third, me], false, &me, &peer));
        assert!(!valid_initial_pair(&[me, me], false, &me, &me));
    }

    /// A stranger's Welcome opens a silent request only while requests are on, never a group. Only
    /// the peer ends the accepted pair, and blocking drops their next Welcome.
    #[tokio::test]
    async fn stranger_welcome_is_a_silent_request_until_accepted() {
        async fn welcome(
            from: &Device, to: &Device, dht: &FakeDhtClient, gid: [u8; 32],
            meta: Option<&GroupMeta>,
        ) -> WelcomeEnvelopeP {
            let fetched = fetch_verified_keypackage(&from.ctx(dht), &to.ipk, false).await;
            let (kp, kp_ref) = fetched.unwrap();
            let (leaf, credential) = build_self_credential(&from.signer).unwrap();
            leaf.store(from.provider.storage()).unwrap();
            let (provider, leaf) = (&from.provider, &leaf);
            let mut group = MlsGroupHandle::create(provider, leaf, credential, &gid, meta).unwrap();
            let (_, welcome) = group.add_members(provider, leaf, &[kp]).unwrap();
            group.merge_pending_commit(provider).unwrap();
            make_welcome_envelope(welcome, gid, from.ipk, to.ipk, kp_ref, &from.signer).unwrap()
        }
        let scope = ScopedCore::new();
        let (bob, alice, mallory) = (Device::new(0xB1), Device::new(0xA1), Device::new(0xE1));
        identity(&scope.core.db.identity().lock(), 0xB1);
        let dht = FakeDhtClient::default();
        for _ in 0..4 {
            bob.publish_keypackage(&dht).await;
        }
        let ctx = bob.ctx(&dht);
        let admit = |from: &Device, envelope| {
            process_welcome_inbound(&ctx, from.ipk, envelope, None).unwrap()
        };

        crate::requests::set_message_requests_enabled(false).unwrap();
        let first = welcome(&alice, &bob, &dht, [0xA2; 32], None).await;
        assert!(!admit(&alice, first.clone()));
        assert_eq!(Contact::status(&alice.ipk), None);
        crate::requests::set_message_requests_enabled(true).unwrap();
        assert!(admit(&alice, first));
        let chat = Conversation::for_peer(&alice.ipk).unwrap();
        assert!(crate::requests::is_request_chat(&chat));
        assert!(crate::api::messaging::get_conversation(chat.to_vec()).unwrap().unwrap().request);
        assert!(crate::api::messaging::get_contacts().iter().all(|c| c.ipk != alice.ipk));
        let refused = |e: anyhow::Error| {
            e.to_string().contains("request") || panic!("not the request gate: {e}")
        };
        assert!(refused(send_control(chat, AppPayload::PairAck).await.unwrap_err()));
        assert!(refused(react(chat, [1; 16], "👍".into(), true).await.unwrap_err()));
        let meta = GroupMeta::founded("Spam".into(), alice.ipk);
        assert!(!admit(&alice, welcome(&alice, &bob, &dht, [0xA3; 32], Some(&meta)).await));

        crate::requests::accept_message_request(alice.ipk.to_vec()).unwrap();
        assert!(Contact::is_paired(&alice.ipk) && !crate::requests::is_request_chat(&chat));
        let group_chat = Conversation::join_group(&alice.ipk, &[alice.ipk, bob.ipk]).unwrap();
        Conversation::bind_group(&group_chat, &[0xA4; 32]).unwrap();
        let carol = [0xC1; 32];
        Contact::save(carol, "Carol".into()).unwrap();
        unpaired(group_chat, alice.ipk);
        unpaired(chat, carol);
        assert!(Contact::is_paired(&alice.ipk) && Contact::is_paired(&carol));
        assert!(Conversation::group_of(&chat).is_some());
        unpaired(chat, alice.ipk);
        assert_eq!(Contact::status(&alice.ipk), Some(crate::data::contact::PAIR_STATUS_PENDING));
        assert!(Conversation::group_of(&chat).is_none());
        assert!(Conversation::group_of(&group_chat).is_some());

        assert!(admit(&mallory, welcome(&mallory, &bob, &dht, [0xE2; 32], None).await));
        crate::requests::block_message_request(mallory.ipk.to_vec()).await.unwrap();
        assert_eq!(Contact::status(&mallory.ipk), None);
        assert!(!admit(&mallory, welcome(&mallory, &bob, &dht, [0xE3; 32], None).await));
    }
}
