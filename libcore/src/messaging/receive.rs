use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use common::crypto::verify_ed25519;
use common::crypto::verify_versioned;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::Body;
use common::proto::mls_wire::MAX_FRAMED_MLS_BYTES;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MlsApplicationEnvelopeP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::PairDeclineP;
use common::proto::mls_wire::envelope_signing_input;
use common::proto::mls_wire::pair_decline_signing_input;
use common::proto::pack::Unpacker;
use common::utils::now_secs;
use ed25519_dalek::VerifyingKey;
use log::debug;
use log::info;
use log::warn;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::ProtocolMessage;

use super::body::apply_revise_body;
use super::session::MlsContext;
use super::session::heal_dead_group;
use super::welcome::home_for_group;
use super::welcome::process_welcome_inbound;
use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::mls::MlsGroupHandle;
use crate::mls::epoch_catchup::Drained;
use crate::mls::recovery::Received;
use crate::mls::types::MlsGroupError;
use crate::quic::dht_client::DhtClient;
use crate::state::core;

fn is_welcome_envelope(payload: &[u8]) -> bool {
    matches!(
        common::proto::mls_wire::MlsEnvelopeP::deser(payload),
        Ok(common::proto::mls_wire::MlsEnvelopeP::Welcome(_)
            | common::proto::mls_wire::MlsEnvelopeP::GroupWelcome { .. })
    )
}

/// `accepted_at_ms` is stamped by the origin relay outside every signature, so it is clamped to
/// our clock: a home cannot date a message into the future.
pub(crate) fn accepted_at_secs(accepted_at_ms: u64) -> u64 {
    (accepted_at_ms / 1_000).min(now_secs())
}

/// The signature covers `to`, `from`, `id` and the payload, so a relay can neither re-address a
/// captured dispatch at us nor mint one under a contact's IPK.
fn verify_dispatch_sig(our_ipk: &VerifyingKey, msg: &DeliverP) -> Result<()> {
    verify_versioned(&msg.from, &msg.sig.0, |v| {
        dispatch_sig_message(v, our_ipk.as_bytes(), &msg.from, &msg.id.0, &msg.payload)
    })
        .map_err(|e| anyhow!("dispatch signature: {e}"))
}

/// `Ok` means a terminal state: stored, buffered or correctly dropped. `Err` leaves the dispatch
/// unacknowledged.
pub(crate) async fn process_deliver<C: DhtClient>(our_ipk: VerifyingKey, msg: DeliverP, dht: &C) -> Result<()> {
    // Dropped envelopes are acked, not failed: no ack reads as a dead connection and gets us
    // evicted, and a queued envelope would be redelivered forever.
    if let Err(e) = verify_dispatch_sig(&our_ipk, &msg) {
        warn!("MESSAGE: rejected unsigned/forged dispatch from {}: {e}", hex::encode(&msg.from[..4]));
        return Ok(());
    }

    // Already decrypted via another home: ack but never re-decrypt, since the ratchet key is
    // spent. Keyed on the outer envelope, so it covers every payload kind.
    if crate::data::seen::Seen::contains(&msg.from, &msg.id.0) {
        return Ok(());
    }

    // Drop non-Welcome envelopes from senders we share no chat with. A stranger's Welcome may be
    // a first pair, so the invite gate downstream decides.
    if !is_welcome_envelope(&msg.payload)
        && !Contact::exists(&msg.from)
        && !Conversation::shares_a_chat_with(&msg.from)
    {
        info!("MESSAGE: dropped envelope from unknown sender {}", hex::encode(&msg.from[..4]));
        return Ok(());
    }

    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = core().db.mls();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht,
    };
    let result = process_inbound_envelope(
        &ctx,
        *msg.from,
        &msg.payload,
        msg.accepted_at_ms,
        msg.id.0,
    )
    .await;

    let attempts = match &result {
        Err(e) if !crate::utils::is_storage_error(e) => failed_attempts(&msg.from, &msg.id.0),
        _ => 0,
    };
    let disposition = disposition(&result, attempts);
    let from = hex::encode(&msg.from[..4]);
    match &result {
        Ok(InboundDecoded::ApplicationNoGroup { .. }) => {
            // `messaging` already asked the sender to re-establish.
            warn!("MESSAGE: dropped message for dead group from {from}; re-establishment fired")
        },
        Ok(InboundDecoded::ApplicationStale) => {
            warn!("MESSAGE: stale-epoch envelope from {from}; dropping")
        },
        Err(e) if disposition == Disposition::Settle => {
            warn!("MESSAGE: dropping an undeliverable dispatch from {from} after {attempts} attempts: {e}")
        },
        Err(e) if crate::utils::is_storage_error(e) => {
            warn!("MESSAGE: retaining dispatch from {from}: {e:#}")
        },
        Err(e) => warn!("MESSAGE: process_inbound_envelope failed from {from}: {e}"),
        Ok(_) => {},
    }
    if let (Disposition::Retain, Err(e)) = (disposition, &result) {
        bail!("process failed: {e}");
    }
    if disposition == Disposition::Settle {
        crate::data::seen::Seen::record(&msg.from, &msg.id.0, now_secs());
    }
    Ok(())
}

/// What happens to a processed dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// Left unacknowledged, so the relay redelivers it.
    Retain,
    Ack,
    /// Acknowledged and recorded, so a copy from another home is acked without processing.
    Settle,
}

/// `attempts` counts this dispatch's failures, this one included. A storage failure is retried
/// forever, any other one `MAX_PROCESS_ATTEMPTS` times.
fn disposition(result: &Result<InboundDecoded>, attempts: u8) -> Disposition {
    match result {
        Ok(InboundDecoded::Welcome | InboundDecoded::WelcomeDropped | InboundDecoded::PairDeclined) => {
            Disposition::Settle
        },
        // A spent sender-ratchet secret, a dead group and a stale epoch never decrypt later.
        Ok(
            InboundDecoded::ApplicationBuffered
            | InboundDecoded::ApplicationUndecryptable
            | InboundDecoded::ApplicationNoGroup { .. }
            | InboundDecoded::ApplicationStale,
        ) => Disposition::Ack,
        Err(e) if crate::utils::is_storage_error(e) => Disposition::Retain,
        Err(_) if attempts < MAX_PROCESS_ATTEMPTS => Disposition::Retain,
        Err(_) => Disposition::Settle,
    }
}

const MAX_PROCESS_ATTEMPTS: u8 = 3;

/// Count one more failed attempt for a dispatch; a new entry evicts the map when it grows large.
fn failed_attempts(from: &[u8; 32], id: &[u8; 16]) -> u8 {
    let mut failed = core().messaging.failed_dispatches.lock();
    if failed.len() >= 1024 && !failed.contains_key(&(*from, *id)) {
        failed.clear();
    }
    let attempts = failed.entry((*from, *id)).or_insert(0);
    *attempts = attempts.saturating_add(1);
    *attempts
}

/// `sender_ipk` and `dispatch_id` must come from the verified dispatch, whose signature covers
/// them and the payload.
pub async fn process_inbound_envelope<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], payload: &[u8], accepted_at_ms: u64,
    dispatch_id: [u8; 16],
) -> Result<InboundDecoded> {
    let envelope =
        MlsEnvelopeP::deser(payload).map_err(|e| anyhow!("postcard deser MlsEnvelopeP: {e}"))?;

    // Size caps before openmls touches the bytes, so an oversize blob cannot amplify parsing cost.
    match &envelope {
        MlsEnvelopeP::Welcome(env) | MlsEnvelopeP::GroupWelcome { welcome: env, .. }
        | MlsEnvelopeP::GroupMigrationWelcome { welcome: env, .. } => {
            if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
                bail!(
                    "inbound welcome_blob {} exceeds MAX_WELCOME_BYTES = {}",
                    env.welcome_blob.0.len(),
                    MAX_WELCOME_BYTES
                );
            }
        },
        MlsEnvelopeP::Application(env) | MlsEnvelopeP::GroupApplication { message: env, .. } => {
            if env.mls_message.0.len() > MAX_FRAMED_MLS_BYTES {
                bail!(
                    "inbound mls_message {} exceeds MAX_FRAMED_MLS_BYTES = {}",
                    env.mls_message.0.len(),
                    MAX_FRAMED_MLS_BYTES
                );
            }
        },
        MlsEnvelopeP::GroupMemberRequest { .. } | MlsEnvelopeP::PairDecline(_) => {}, /* fixed-size, no cap */
        MlsEnvelopeP::GroupMigrationReady { .. } => {},
        MlsEnvelopeP::ContactRequest { .. } => {},
    }

    match envelope {
        MlsEnvelopeP::GroupMigrationReady { group, branch, approval } => {
            crate::groups::migration::received(group.0, branch.0, approval)?;
            Ok(InboundDecoded::ApplicationBuffered)
        },
        MlsEnvelopeP::GroupMigrationWelcome { group, branch, approvals, welcome, history, signature } => {
            crate::groups::migration::accept(group.0, branch.0, sender_ipk, &approvals,
                &welcome, &history.0, &signature.0)?;
            // This updates an existing group, not a contact pairing. Do not
            // create a direct chat with its founder through the PairAck path.
            Ok(InboundDecoded::ApplicationBuffered)
        },
        MlsEnvelopeP::GroupMemberRequest { group, request } => {
            crate::groups::member_requests::received(group.0, request)?;
            Ok(InboundDecoded::ApplicationBuffered)
        },
        MlsEnvelopeP::GroupWelcome { welcome, history, signature } => {
            ensure!(
                history.0.len() <= common::proto::mls_wire::MAX_WELCOME_BYTES,
                "invitation history too long"
            );
            let input = common::proto::mls_wire::group_welcome_signing_input(&welcome, &history.0);
            verify_ed25519(&sender_ipk, &input, &signature.0)?;
            Ok(if process_welcome_inbound(ctx, sender_ipk, welcome, Some(&history.0))? {
                InboundDecoded::Welcome
            } else {
                InboundDecoded::WelcomeDropped
            })
        },
        MlsEnvelopeP::GroupApplication { branch, message, proof } => {
            let ours = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
            Ok(crate::groups::recovery::receive(
                ctx.provider,
                &ours,
                sender_ipk,
                branch.0,
                message,
                proof.map(|p| p.0),
                dispatch_id,
                accepted_at_ms,
            )?)
        },
        MlsEnvelopeP::Welcome(env) => Ok(if process_welcome_inbound(ctx, sender_ipk, env, None)? {
            InboundDecoded::Welcome
        } else {
            InboundDecoded::WelcomeDropped
        }),
        MlsEnvelopeP::Application(env) => {
            let decoded = process_application_inbound(ctx, sender_ipk, env, accepted_at_ms, dispatch_id)?;
            if let InboundDecoded::ApplicationNoGroup { group_id } = &decoded {
                heal_dead_group(ctx, sender_ipk, group_id).await;
            }
            Ok(decoded)
        },
        // Retired with message requests. Dropped like a refused Welcome, which
        // acknowledges it, so a queued one cannot hold up the drain.
        MlsEnvelopeP::ContactRequest { .. } => Ok(InboundDecoded::WelcomeDropped),
        MlsEnvelopeP::PairDecline(d) => {
            process_pair_decline_inbound(sender_ipk, d)?;
            Ok(InboundDecoded::PairDeclined)
        },
    }
}

/// Verified so a malicious relay cannot forge a rejection to grief a pair.
fn process_pair_decline_inbound(sender_ipk: [u8; 32], d: PairDeclineP) -> Result<()> {
    if d.sender_ipk.0 != sender_ipk {
        bail!("pair-decline sender_ipk mismatch");
    }
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    if d.recipient_ipk.0 != our_ipk {
        bail!("pair-decline not addressed to us");
    }
    let msg = pair_decline_signing_input(&sender_ipk, &our_ipk, d.reason, d.timestamp);
    verify_ed25519(&sender_ipk, &msg, &d.sig.0)
        .map_err(|_| anyhow!("pair-decline signature invalid"))?;
    if !Contact::exists(&sender_ipk) {
        bail!("pair-decline from non-contact");
    }
    // A live group is ground truth: a decline for a late or redundant handshake must not tear down
    // a working pair. Only a never-completed, group-less pair is rejectable.
    if Contact::get(&sender_ipk).is_some_and(|c| c.inner.mls_group_id.is_some()) {
        warn!(
            "PAIR: ignoring decline (reason {}) from already-paired {}",
            d.reason,
            hex::encode(&sender_ipk[..4])
        );
        return Ok(());
    }
    Contact::mark_rejected(&sender_ipk, d.reason);
    if let Ok(conversation) = Conversation::for_peer(&sender_ipk) {
        Message::mark_all_failed_in(&conversation);
    }
    warn!("PAIR: {} declined (reason {})", hex::encode(&sender_ipk[..4]), d.reason);
    Ok(())
}

#[derive(Debug)]
pub enum InboundDecoded {
    Welcome,
    /// Refused: ack and discard.
    WelcomeDropped,
    /// Delivered, buffered for a future epoch, or consumed without producing a message.
    ApplicationBuffered,
    /// Can never apply: a retired envelope, a stale or far-future epoch, a refused commit, or a
    /// sender or payload the group does not permit.
    ApplicationStale,
    /// Application message whose sender-ratchet secret was already discarded.
    ApplicationUndecryptable,
    /// No local state for the group: the ciphertext is unrecoverable, so the caller must ack it.
    /// `heal_dead_group` re-establishes a known contact's pair.
    ApplicationNoGroup { group_id: [u8; 32] },
    /// The invitee declined our pair, already applied; the caller just acks.
    PairDeclined,
}

fn process_application_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: MlsApplicationEnvelopeP,
    accepted_at_ms: u64, dispatch_id: [u8; 16],
) -> Result<InboundDecoded> {
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    process_application_inbound_for(ctx, sender_ipk, &our_ipk, env, accepted_at_ms, dispatch_id)
}

/// Needs a verified outer dispatch: the inner envelope signature does not cover `dispatch_id`.
pub fn process_application_inbound_for<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], our_ipk: &[u8; 32], env: MlsApplicationEnvelopeP,
    accepted_at_ms: u64, dispatch_id: [u8; 16],
) -> Result<InboundDecoded> {
    verify_versioned(&sender_ipk, &env.sender_sig.0, |v| {
        envelope_signing_input(v, our_ipk, &env.group_id.0, env.epoch, &env.mls_message.0)
    })
        .map_err(|_| anyhow!("application envelope sig invalid"))?;

    // A recovery-enabled group must publish every ratchet and commit through
    // its journal. The retired envelope cannot bypass branch validation.
    if crate::mls::recovery::registered(ctx.provider, &env.group_id.0)?
        || crate::mls::migration::completed(ctx.provider, &env.group_id.0)?.is_some() {
        return Ok(InboundDecoded::ApplicationStale);
    }

    // No local state is a typed outcome, not an error, so the caller acks instead of being
    // redelivered forever.
    let operation = crate::mls::recovery::operation_lock(&env.group_id.0).lock();
    let Some(mut group) = MlsGroupHandle::load(ctx.provider, &env.group_id.0)
        .map_err(|e| anyhow!("load group: {e}"))?
    else {
        warn!(
            "MLS: no local state for group {} (sender {})",
            hex::encode(&env.group_id.0[..4]),
            hex::encode(&sender_ipk[..4])
        );
        return Ok(InboundDecoded::ApplicationNoGroup { group_id: env.group_id.0 });
    };

    let current = group.epoch();
    if env.epoch > current {
        // Refuse envelopes more than `MAX_EPOCH_AHEAD` epochs ahead: we cannot catch up that far,
        // and a malicious member could otherwise pin buffer memory for free.
        let delta = env.epoch - current;
        if delta > common::proto::mls_wire::MAX_EPOCH_AHEAD {
            warn!(
                "MLS: dropping far-future envelope (delta={} > MAX_EPOCH_AHEAD={}) for group {}",
                delta,
                common::proto::mls_wire::MAX_EPOCH_AHEAD,
                hex::encode(&env.group_id.0[..4])
            );
            return Ok(InboundDecoded::ApplicationStale);
        }
        // Buffer for catchup later. A full buffer is not an ack: the relay
        // keeps its copy.
        let pushed = ctx
            .buffer
            .push_dispatch(
                &group,
                env.mls_message.0.clone(),
                env.epoch,
                sender_ipk,
                dispatch_id,
                accepted_at_ms,
            )
            .map_err(|e| anyhow!("epoch-ahead buffer push: {e}"))?;
        if pushed == crate::mls::epoch_catchup::PushOutcome::Discarded {
            bail!("epoch-ahead buffer full for group {}", hex::encode(&env.group_id.0[..4]));
        }
        return Ok(InboundDecoded::ApplicationBuffered);
    }
    if env.epoch < current {
        // Stale: `process_incoming` would error and the relay redeliver forever, so return a typed
        // outcome the caller acks.
        warn!(
            "MLS: dropping stale-epoch envelope (env={} < current={}) for group {}",
            env.epoch,
            current,
            hex::encode(&env.group_id.0[..4])
        );
        return Ok(InboundDecoded::ApplicationStale);
    }

    use openmls::prelude::tls_codec::Deserialize as _;
    let epoch_before = group.epoch();
    let in_msg = openmls::prelude::MlsMessageIn::tls_deserialize_exact(&env.mls_message.0)
        .map_err(|e| anyhow!("MlsMessageIn deser: {e:?}"))?;
    let proto: ProtocolMessage =
        in_msg.try_into_protocol_message().map_err(|e| anyhow!("not a ProtocolMessage: {e:?}"))?;

    // A commit is processed and merged in one storage operation, and a permitted plaintext is
    // staged in the one that decrypts it, so a failure leaves the group at its epoch and the
    // redelivery still decrypts.
    let gid = env.group_id.0;
    let mut merging = false;
    let processed = ctx.provider.storage().atomic(|| -> Result<_, MlsGroupError> {
        let processed = match group.process_incoming(ctx.provider, proto) {
            Err(MlsGroupError::UnboundSender) => return Ok(None),
            processed => processed?,
        };
        // The MLS leaf credential says who wrote this; the outer sender only proves who put it on
        // the wire. They coincide in a pair and routinely diverge in a group.
        let author = processed.sender;
        let inbound = match processed.content {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                merging = true;
                Inbound::Commit(group.merge_staged_commit_if_permitted(
                    ctx.provider,
                    *staged,
                    author,
                )?)
            },
            ProcessedMessageContent::ApplicationMessage(app) => {
                let payload = app.into_bytes();
                let staged = group.application_is_permitted(&author, &payload);
                if staged {
                    let live = Received { author, id: dispatch_id, accepted_at_ms, payload };
                    ctx.provider.storage().with_conn(|conn| live.stage(conn, &gid, &[0; 32]))?;
                }
                Inbound::Message { staged }
            },
            content => Inbound::Content(content),
        };
        Ok(Some((author, inbound)))
    });
    let (author, inbound) = match processed {
        Ok(Some(processed)) => processed,
        Ok(None) => {
            warn!(
                "GROUP: dropped a message from an unbound leaf in {}",
                hex::encode(&env.group_id.0[..4])
            );
            return Ok(InboundDecoded::ApplicationStale);
        },
        Err(err) if merging => return Err(anyhow!("merge_staged_commit: {err}")),
        Err(err) if err.is_spent_secret() => {
            // Its plaintext was staged with the decrypt, and applying it may have failed since.
            if let Some(conversation) = Conversation::for_group(&gid) {
                crate::groups::recovery::deliver_plaintext(ctx.provider, gid, conversation)?;
            }
            return Ok(InboundDecoded::ApplicationUndecryptable);
        },
        Err(err) => return Err(err.into()),
    };

    let (live, changed) = match inbound {
        Inbound::Message { staged: true } => (true, None),
        // Every honest device refuses it alike, and a refused commit holds the epoch for all but
        // the committer. Acked as stale: redelivery would only be refused again.
        Inbound::Message { staged: false }
        | Inbound::Commit(crate::mls::CommitOutcome::Refused) => {
            return Ok(InboundDecoded::ApplicationStale);
        },
        Inbound::Commit(crate::mls::CommitOutcome::Merged(changed)) => (false, changed),
        Inbound::Content(ProcessedMessageContent::ProposalMessage(p)) => {
            // A self-removal proposal in a pre-rules group is a leave the founder commits inline: a
            // commit by reference would fork off every member who missed the proposal.
            use openmls::prelude::Proposal;
            use openmls::prelude::Sender;
            let leaver = match (p.sender(), p.proposal()) {
                (Sender::Member(i), Proposal::Remove(r)) if *i == r.removed() => {
                    group.member_ipk_at(*i)
                },
                _ => None,
            };
            if let Some(who) = leaver
                && group.group_meta().is_some_and(|m| m.state.is_none() && m.founder == *our_ipk)
                && let Some(conversation) = Conversation::for_group(&env.group_id.0)
            {
                core().spawn(async move {
                    if let Err(e) = crate::groups::carry_leave(conversation, who).await {
                        warn!("GROUP: could not carry a leave: {e}");
                    }
                });
            }
            return Ok(InboundDecoded::ApplicationBuffered);
        },
        // External join proposals; commits were merged above.
        Inbound::Content(_) => return Ok(InboundDecoded::ApplicationBuffered),
    };
    // Read the roster after draining, since a drained commit may have moved it. The drain stages
    // its messages and the live one was staged with its decrypt, so a failure below loses neither.
    let drained = ctx.buffer.drain_when_ready(&mut group, ctx.provider);
    // A restored group may outlive its messages-DB mapping, so resolve its real chat, and only
    // after every group mutation: homing can spawn an introduction.
    let conversation = home_for_group(&group, &author)?;
    sync_group_from(&group, &gid);
    if let Some(changed) = changed {
        crate::groups::changed(
            conversation,
            &changed,
            accepted_at_secs(accepted_at_ms),
        )?;
    }
    // Drained copies are recorded before applying, so a copy from another home is caught before
    // decrypt. Older buffer rows carry a ciphertext-derived id instead of the dispatch id.
    let now = now_secs();
    for drained in drained {
        match drained {
            Drained::Message(m) => {
                let id = <[u8; 16]>::try_from(m.dispatch_id.as_slice());
                if let (Some(from), Ok(id)) = (m.dispatch_sender, id) {
                    crate::data::seen::Seen::record(&from, &id, now);
                }
            },
            Drained::Change { changed, accepted_at_ms } => {
                let ts = accepted_at_secs(accepted_at_ms);
                if let Err(e) = crate::groups::changed(conversation, &changed, ts) {
                    warn!("GROUP: could not record a buffered change: {e}");
                }
            },
        }
    }
    if live {
        // Any decryptable message proves the pair works, confirming a pending contact.
        Contact::mark_paired(&sender_ipk);
    }
    crate::groups::recovery::deliver_plaintext(ctx.provider, gid, conversation)?;
    if live {
        // Only once applied: until then its redelivery is what finishes applying it.
        crate::data::seen::Seen::record(&sender_ipk, &dispatch_id, now);
    }
    drop(operation);
    crate::data::receipts::schedule();
    if group.epoch() != epoch_before {
        crate::groups::resume(conversation);
    }
    Ok(InboundDecoded::ApplicationBuffered)
}

/// What processing one message produced: an application message, staged when its author may send
/// it, the outcome of merging a commit, or other content.
enum Inbound {
    Message { staged: bool },
    Commit(crate::mls::CommitOutcome),
    Content(ProcessedMessageContent),
}

/// The merged tree says who is in the group and its context who runs it, not the commit's
/// narration. A removed member keeps an inactive row so old messages still resolve to a name.
fn sync_group_from(group: &MlsGroupHandle, group_id: &[u8; 32]) {
    if let Some(conversation) = Conversation::for_group(group_id)
        && let Err(e) =
            Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())
    {
        warn!("GROUP: could not sync the roster after a commit: {e}");
    }
}

/// The single application handler for live delivery, catch-up, and recovery.
/// Call only after MLS has authenticated and authorized the plaintext at its epoch.
pub(crate) fn receive_application_content(
    conv: [u8; 16], author: [u8; 32], dispatch_id: [u8; 16], accepted_at_ms: u64, plaintext: &[u8],
) -> Result<()> {
    let payload = AppPayload::deser(plaintext);
    if crate::groups::delete_pending(&conv)
        && !matches!(
            &payload,
            Ok(AppPayload::GroupRequest(_)
                | AppPayload::GroupWelcome { .. }
                | AppPayload::GroupInvitation { .. })
        )
    {
        return Ok(());
    }
    match payload {
        Ok(AppPayload::GroupAdmins { .. }) => {},
        // `Post` carries the quote target beside the body; older payloads convert to the same pair.
        Ok(
            p @ (AppPayload::Post { .. }
            | AppPayload::Text(..)
            | AppPayload::Reply { .. }
            | AppPayload::Image { .. }
            | AppPayload::Attachment { .. }),
        ) => {
            let pair = match p {
                AppPayload::Post { reply_to, body } => Some((reply_to, body)),
                other => crate::messaging::body::legacy_body(other),
            };
            let Some((reply_to, body)) = pair else {
                warn!("MESSAGE: content payload with no body from {}", hex::encode(&author[..4]));
                bail!("bad content payload");
            };
            let did = dispatch_id;
            let timestamp = accepted_at_secs(accepted_at_ms);
            let auto = match &body {
                Body::Attachment { size, file_id, .. } => Some((*size, *file_id)),
                _ => None,
            };
            let sticker = match &body {
                Body::Sticker { pack, id, token, store, .. } => {
                    Some(common::proto::sticker::StickerRef {
                        pack:  *pack,
                        id:    *id,
                        token: *token,
                        store: *store,
                    })
                },
                _ => None,
            };
            match crate::messaging::body::save_inbound_body(
                &conv, &author, &did, timestamp, reply_to, body,
            ) {
                Ok(Some((saved, content))) => {
                    MessageEv::Received {
                        id: saved.inner.id,
                        conversation: conv,
                        sender: author,
                        content,
                        timestamp,
                    }
                    .emit();
                    info!("MESSAGE: received from {}", hex::encode(&author[..4]));
                    // Event time was persisted with the incoming message.
                    crate::data::receipts::schedule();
                    let from = author;
                    // Fetch the bytes without a tap only from a paired contact over a trusted
                    // network; otherwise the UI drives the pull.
                    if let Some((size, file_id)) = auto {
                        if crate::transfer::should_auto_download(&from, size, false) {
                            core().spawn(async move {
                                let _ = crate::transfer::download(file_id).await;
                            });
                        }
                    }
                    // A sticker is small and named by hash: fetch it now so
                    // the chat opens on the picture, not on a fetch.
                    if let Some(r) = sticker {
                        core().spawn(async move {
                            if let Err(e) = crate::stickers::fetch(&r).await {
                                debug!("STICKERS: prefetch failed: {e:#}");
                            }
                        });
                    }
                },
                // Relay redelivered a dispatch_id we already stored: no
                // re-emit, but still Ok so the caller acks and the relay GCs.
                Ok(None) => {
                    debug!("MESSAGE: duplicate from {}, already stored", hex::encode(&author[..4]));
                },
                Err(e) => {
                    warn!("MESSAGE: failed to save incoming: {e}");
                    return Err(e.context("save failed"));
                },
            }
        },
        Ok(AppPayload::AttachmentSharing(offer)) => {
            if let Err(e) = crate::transfer::sharing::receive(conv, author, offer) {
                warn!("TRANSFER: sharing grant rejected: {e}");
            }
        },
        Ok(payload @ (AppPayload::Receipt { .. } | AppPayload::ReceiptDetails(_))) => {
            crate::data::receipts::receive(&conv, &author, payload)?;
        },
        Ok(
            payload @ (AppPayload::Edit { .. }
            | AppPayload::Revise { .. }
            | AppPayload::Delete { .. }),
        ) => {
            receive_message_mutation(&conv, &author, payload)?;
        },
        Ok(AppPayload::React { target, emoji, add }) => {
            // `author` is the MLS-authenticated sender, so group reactions attribute correctly.
            let ts = accepted_at_secs(accepted_at_ms);
            if crate::data::reaction::Reaction::apply(&conv, &target, &author, &emoji, add, ts) {
                crate::events::messaging::ReactionEv {
                    conversation: conv,
                    dispatch_id: target,
                    reactor: author,
                    emoji,
                    add,
                }
                .emit();
            }
        },
        Ok(AppPayload::System(event)) => {
            use common::proto::mls_wire::SystemEvent;

            let ts = accepted_at_secs(accepted_at_ms);
            let (code, actor, target) = crate::groups::system_row(&event, author);
            // A rename has no Commit behind it, so the event is the change. Membership events only
            // narrate; the merged Commit is what moved the roster.
            if let SystemEvent::Titled { title } = &event {
                // Only a group has a shared name; a rename from the wire must not relabel a DM.
                let is_group = Conversation::get(&conv)
                    .is_some_and(|c| c.kind == crate::data::conversation::KIND_GROUP);
                if !is_group {
                    warn!("GROUP: ignored a rename aimed at a direct chat");
                } else {
                    Conversation::set_title(&conv, title)?;
                }
            }
            // Someone who joined after us never heard our introduction, so repeat it to them alone.
            if let SystemEvent::Added { who } = &event {
                if who.0
                    != crate::data::identity::Identity::local_ipk().unwrap_or_default()
                {
                    crate::messaging::welcome::introduce_ourselves_to(conv, who.0);
                }
            }
            match Message::save_system(conv, actor, &dispatch_id, code, &target, ts, false) {
                Ok(Some(row)) => MessageEv::Received {
                    id:           row.inner.id,
                    conversation: conv,
                    sender:       actor,
                    content:      target,
                    timestamp:    ts,
                }
                .emit(),
                Ok(None) => debug!("GROUP: duplicate system event, already stored"),
                Err(e) => return Err(e),
            }
        },
        Ok(AppPayload::Profile { name }) => {
            // Their claim about themselves, kept apart from the address book so it never overwrites
            // a name we chose.
            if !crate::profile_sync::store::has_field(&core().db.messages().lock(), &author, common::proto::profile::Field::Name) {
                crate::data::peer_name::put(&author, &name)?;
            }
        },
        Ok(
            payload @ (AppPayload::ProfileDetails { .. }
            | AppPayload::ProfileDetailsSync { .. }
            | AppPayload::ProfileDetailsAck { .. }),
        ) => {
            crate::profile_sync::receive(conv, author, payload);
        },
        Ok(AppPayload::GroupPicture { revision, avif }) => {
            if let Err(e) = crate::data::group_picture::receive_authorized(conv, revision, avif) {
                log::warn!("GROUP: picture rejected: {e}");
            }
        },
        Ok(
            payload @ (AppPayload::Avatar { .. }
            | AppPayload::AvatarSync { .. }
            | AppPayload::AvatarAck { .. }),
        ) => {
            crate::profile_sync::receive(conv, author, payload);
        },
        Ok(AppPayload::Unpaired) => crate::messaging::welcome::unpaired(conv, author),
        Ok(AppPayload::GroupRequest(request)) => crate::groups::requested(conv, author, request),
        Ok(AppPayload::GroupInvitation { who, kp_ref, welcome, history }) => {
            crate::groups::forward_welcome(conv, author, who.0, kp_ref.0, welcome, Some(history))?;
        },
        Ok(AppPayload::GroupWelcome { who, kp_ref, welcome }) => {
            crate::groups::forward_welcome(conv, author, who.0, kp_ref.0, welcome, None)?;
        },
        Ok(AppPayload::PairAck) => {
            // Proof of pair: delivery already marked the contact paired.
            info!("PAIR: confirmed by {}", hex::encode(&author[..4]));
            // The pair now works both ways, and they hold our name from
            // the invite: our picture is the one thing left to show them.
            crate::messaging::welcome::introduce_ourselves(conv);
        },
        Ok(AppPayload::P2pOffer {
            session,
            in_reply_to,
            expires_at_ms,
            candidates,
            relay,
            token,
            disco_key,
        }) => {
            // Routed to the waiting P2P session, never stored.
            info!(
                "P2P[{}]: received offer — {} cands",
                hex::encode(&author[..4]),
                candidates.len()
            );
            crate::p2p::deliver_offer(
                author,
                crate::p2p::Offer {
                    session,
                    in_reply_to,
                    expires_at_ms,
                    candidates,
                    relay,
                    token,
                    disco_key,
                },
            );
        },
        Ok(AppPayload::P2p { .. }) => {
            // A sender this old cannot read our answer, so there is
            // nothing to do with its offer but let it go.
            debug!("P2P[{}]: legacy offer ignored", hex::encode(&author[..4]));
        },
        Ok(AppPayload::FileWant { file_id }) => {
            // Reverse wake, never stored: the push already revived us, so bring the P2P listener up
            // for the receiver's retry dial.
            info!("P2P: FileWant received from {}", hex::encode(&author[..4]));
            crate::transfer::on_file_want(author, file_id);
        },
        Ok(AppPayload::Call(signal)) => {
            // Routed to the call engine, never stored. Only the direct chat with
            // the caller carries a call, so the answer goes to them alone.
            use crate::data::conversation::Conversation;
            let direct = Conversation::get(&conv)
                .is_some_and(|c| c.kind == crate::data::conversation::KIND_DIRECT)
                && Conversation::peer_of(&conv) == Some(author);
            if direct {
                crate::call::on_signal(author, conv, signal);
            } else {
                debug!("CALL: signal from {} outside a direct chat ignored", hex::encode(&author[..4]));
            }
        },
        Err(e) => {
            warn!("MESSAGE: undecodable AppPayload from {}: {e}", hex::encode(&author[..4]));
            // The ciphertext is already consumed. A newer app may send a variant we do not know;
            // discard it without tearing down the relay connection.
        },
    }
    Ok(())
}

/// The MLS leaf author, never the outer carrier, owns the target. A delete is kept even before its
/// target arrives; edits and revisions need the body to enforce the revision matrix.
pub(crate) fn receive_message_mutation(
    conversation: &[u8; 16], author: &[u8; 32], payload: AppPayload,
) -> Result<()> {
    match payload {
        AppPayload::Edit { target, content } => {
            if let Some(row) = Message::apply_edit(conversation, &target, &content, false, Some(author)) {
                MessageEv::Edited { id: row.id, conversation: *conversation, content }.emit();
            }
        },
        AppPayload::Revise { target, body } => {
            crate::transfer::sharing::revoke(conversation, author, &target)?;
            if let Some((row, content)) = apply_revise_body(conversation, &target, body, false, Some(author))? {
                MessageEv::Edited { id: row.id, conversation: *conversation, content }.emit();
            }
        },
        AppPayload::Delete { target } => {
            if let Some(row) = Message::receive_delete(conversation, &target, author)? {
                MessageEv::Deleted { id: row.id, conversation: *conversation }.emit();
            }
        },
        _ => bail!("not a message mutation"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use common::PROTOCOL_VERSION;
    use common::proto::mls_wire::AttachmentSharing;
    use common::proto::mls_wire::MAX_EPOCH_AHEAD;
    use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
    use common::proto::mls_wire::ReceiptDetails;
    use common::proto::mls_wire::ReceiptEntry;
    use common::proto::mls_wire::WelcomeEnvelopeP;
    use common::proto::pack::Packer;
    use common::types::bytes::ByteVec;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;
    use openmls::prelude::MlsMessageIn;
    use openmls::prelude::tls_codec::Deserialize as _;

    use super::*;
    use crate::data::message::next_dispatch_id;
    use crate::messaging::body::save_inbound_body;
    use crate::messaging::send::SealedMessage;
    use crate::messaging::send::seal_application_message;
    use crate::messaging::session::lazy_create_group;
    use crate::messaging::session::leaf_signer_for_group;
    use crate::mls::EpochCatchupBuffer;
    use crate::mls::GroupMeta;
    use crate::mls::KeyPackageStash;
    use crate::test_support::ScopedCore;
    use crate::test_support::data::identity;
    use crate::test_support::mls::Party;
    use crate::test_support::mls::found;
    use crate::test_support::mls::with_failing_trigger;
    use crate::test_support::net::Device;
    use crate::test_support::net::FakeDhtClient;
    use crate::test_support::net::pair;

    fn storage_failure() -> Result<InboundDecoded> {
        Err(anyhow::Error::new(rusqlite::Error::InvalidQuery).context("save incoming"))
    }

    fn refused() -> Result<InboundDecoded> {
        Err(anyhow!("undecodable envelope"))
    }

    /// A dispatch that keeps failing is retried a few times and then settled, so it cannot hold
    /// the inbox; a storage failure is retried until it clears.
    #[test]
    fn a_failing_dispatch_is_retried_a_bounded_number_of_times() {
        use Disposition::*;
        let rows = [
            (Ok(InboundDecoded::Welcome), 0, Settle),
            (Ok(InboundDecoded::WelcomeDropped), 0, Settle),
            (Ok(InboundDecoded::PairDeclined), 0, Settle),
            (Ok(InboundDecoded::ApplicationBuffered), 0, Ack),
            (Ok(InboundDecoded::ApplicationUndecryptable), 0, Ack),
            (Ok(InboundDecoded::ApplicationNoGroup { group_id: [0; 32] }), 0, Ack),
            (Ok(InboundDecoded::ApplicationStale), 0, Ack),
            (storage_failure(), 0, Retain),
            (storage_failure(), u8::MAX, Retain),
            (refused(), 1, Retain),
            (refused(), MAX_PROCESS_ATTEMPTS - 1, Retain),
            (refused(), MAX_PROCESS_ATTEMPTS, Settle),
        ];
        for (result, attempts, want) in rows {
            assert_eq!(disposition(&result, attempts), want, "{result:?} after {attempts}");
        }
    }

    #[test]
    fn dispatch_sig_binds_sender_recipient_and_payload() {
        let sender = SigningKey::from_bytes(&[0x11; 32]);
        let me = SigningKey::from_bytes(&[0x22; 32]).verifying_key();
        let deliver = |to: &VerifyingKey| {
            let from = sender.verifying_key().to_bytes();
            let sig =
                sender.sign(&dispatch_sig_message(PROTOCOL_VERSION, to.as_bytes(), &from, &[7; 16], b"envelope"));
            DeliverP {
                id:             [7; 16].into(),
                from:           from.into(),
                payload:        b"envelope".to_vec().into(),
                sig:            sig.to_bytes().into(),
                accepted_at_ms: 0,
                ttl_ms:         0,
            }
        };
        verify_dispatch_sig(&me, &deliver(&me)).unwrap();

        let someone_else = SigningKey::from_bytes(&[0x33; 32]).verifying_key();
        assert!(verify_dispatch_sig(&me, &deliver(&someone_else)).is_err(), "replayed at us");
        let mut rewritten = deliver(&me);
        rewritten.payload = b"other".to_vec().into();
        assert!(verify_dispatch_sig(&me, &rewritten).is_err(), "payload rewritten");
        let mut reattributed = deliver(&me);
        reattributed.from = SigningKey::from_bytes(&[0x44; 32]).verifying_key().to_bytes().into();
        assert!(verify_dispatch_sig(&me, &reattributed).is_err(), "sender minted by the relay");
    }

    #[test]
    fn accepted_at_is_capped_at_the_local_clock() {
        assert_eq!(accepted_at_secs(1_000_999), 1_000);
        let before = now_secs();
        let capped = accepted_at_secs(u64::MAX);
        assert!((before..=now_secs()).contains(&capped));
    }

    /// An envelope that can never decrypt is answered with an outcome the relay acknowledges,
    /// never an error that has it redelivered forever. Only a forged one is an error.
    #[tokio::test]
    async fn envelopes_that_cannot_decrypt_get_a_typed_outcome() {
        let (alice, bob, dht) = (Device::new(3), Device::new(4), FakeDhtClient::default());
        let (mut group, joined) = pair(&alice, &bob, &dht).await;
        // A second pair whose Welcome Bob never processed, as after a restore.
        bob.publish_keypackage(&dht).await;
        let dead = lazy_create_group(&alice.ctx(&dht), &alice.ipk, &alice.signer, &bob.ipk)
            .await
            .unwrap()
            .group_id();
        let leaf = leaf_signer_for_group(&alice.provider, &group, &alice.ipk).unwrap();
        let sealed = seal_application_message(&alice.provider, &mut group, &leaf, b"hi").unwrap();
        let envelope = |gid: [u8; 32], epoch: u64| {
            use ed25519_dalek::Signer;
            let transcript =
                envelope_signing_input(PROTOCOL_VERSION, &bob.ipk, &gid, epoch, &sealed.mls_bytes);
            MlsApplicationEnvelopeP {
                version: MLS_ENVELOPE_VERSION,
                group_id: gid.into(),
                epoch,
                mls_message: ByteVec(sealed.mls_bytes.clone()),
                sender_sig: alice.signer.sign(&transcript).to_bytes().into(),
            }
        };
        let check = |env| {
            process_application_inbound_for(&bob.ctx(&dht), alice.ipk, &bob.ipk, env, 0, [0; 16])
        };
        let buffered = || {
            bob.provider
                .storage()
                .with_conn(|c| {
                    c.query_row("SELECT COUNT(*) FROM mls_epoch_ahead", [], |r| r.get::<_, u32>(0))
                })
                .unwrap()
        };
        let (gid, epoch) = (joined.group_id(), joined.epoch());

        assert!(matches!(
            check(envelope(dead, epoch)),
            Ok(InboundDecoded::ApplicationNoGroup { group_id }) if group_id == dead
        ));
        assert!(matches!(check(envelope(gid, epoch - 1)), Ok(InboundDecoded::ApplicationStale)));
        assert!(matches!(
            check(envelope(gid, epoch + MAX_EPOCH_AHEAD + 1)),
            Ok(InboundDecoded::ApplicationStale)
        ));
        assert_eq!(buffered(), 0, "no copy is kept for an epoch that never comes");
        assert!(matches!(check(envelope(gid, epoch + 1)), Ok(InboundDecoded::ApplicationBuffered)));
        assert_eq!(buffered(), 1, "a near epoch waits for its commit");
        let mut forged = envelope(gid, epoch);
        forged.sender_sig.0[0] ^= 1;
        assert!(check(forged).is_err());

        let oversize = MlsEnvelopeP::Welcome(WelcomeEnvelopeP {
            version:       MLS_ENVELOPE_VERSION,
            group_id:      [0; 32].into(),
            sender_ipk:    alice.ipk.into(),
            recipient_ipk: bob.ipk.into(),
            welcome_blob:  ByteVec(vec![0; MAX_WELCOME_BYTES + 1]),
            kp_ref_used:   [0; 32].into(),
            sender_sig:    [0; 64].into(),
            pairing:       None,
        })
        .ser()
        .unwrap();
        let refused = process_inbound_envelope(&bob.ctx(&dht), alice.ipk, &oversize, 0, [0; 16])
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("MAX_WELCOME_BYTES"), "{refused}");
    }

    /// A commit whose merge fails leaves the group at its epoch, so the redelivery still applies.
    #[tokio::test]
    async fn a_commit_that_fails_to_merge_is_left_whole_for_its_redelivery() {
        let (alice, bob, dht) = (Device::new(7), Device::new(8), FakeDhtClient::default());
        let (mut group, joined) = pair(&alice, &bob, &dht).await;
        let (gid, epoch) = (joined.group_id(), joined.epoch());
        drop(joined);
        let leaf = leaf_signer_for_group(&alice.provider, &group, &alice.ipk).unwrap();
        let params = openmls::prelude::LeafNodeParameters::default();
        let commit =
            group.openmls().self_update(&alice.provider, &leaf, params).unwrap().into_commit();
        let bytes = SealedMessage::from_mls_out(&commit, gid, epoch)
            .unwrap()
            .address_to(&bob.ipk, &alice.signer)
            .unwrap();
        let Ok(MlsEnvelopeP::Application(env)) = MlsEnvelopeP::deser(&bytes) else {
            panic!("not an application envelope")
        };

        // The merge writes the new tree; processing the commit does not.
        let fail_merge = format!(
            "CREATE TEMP TRIGGER fail_merge BEFORE INSERT ON mls_storage WHEN NEW.key_tag = {}
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            crate::mls::storage::tags::TREE
        );
        bob.db.mls().lock().execute_batch(&fail_merge).unwrap();
        let ctx = bob.ctx(&dht);
        assert!(
            process_application_inbound_for(&ctx, alice.ipk, &bob.ipk, env.clone(), 0, [1; 16])
                .is_err()
        );
        bob.db.mls().lock().execute_batch("DROP TRIGGER fail_merge;").unwrap();

        let mut stored = MlsGroupHandle::load(&bob.provider, &gid).unwrap().unwrap();
        assert_eq!(stored.epoch(), epoch);
        let message = MlsMessageIn::tls_deserialize_exact(&env.mls_message.0)
            .unwrap()
            .try_into_protocol_message()
            .unwrap();
        let ProcessedMessageContent::StagedCommitMessage(staged) =
            stored.process_incoming(&bob.provider, message).unwrap().content
        else {
            panic!("not a commit")
        };
        stored.merge_staged_commit(&bob.provider, *staged).unwrap();
        assert_eq!(stored.epoch(), epoch + 1);
    }

    /// A plaintext that fails to stage takes its decrypt back with it, and one that fails to save
    /// stays staged, so a redelivery delivers the message either way.
    #[tokio::test]
    async fn a_message_that_fails_to_stage_or_save_arrives_on_redelivery() {
        let scope = ScopedCore::new();
        let (alice, bob) = (Party::new(0xA6), Party::new(0xB6));
        let gid = [0xD6; 32];
        let meta = GroupMeta::founded("Retry".into(), alice.ipk);
        let (mut group, _) = found(&alice, gid, Some(&meta), [&bob]);
        let conversation = Conversation::join_group(&alice.ipk, &[alice.ipk, bob.ipk]).unwrap();
        Conversation::bind_group(&conversation, &gid).unwrap();
        let stash = KeyPackageStash::new(bob.db.clone());
        let (buffer, dht) = (EpochCatchupBuffer::new(bob.db.clone()), FakeDhtClient::default());
        let ctx = MlsContext { provider: &bob.provider, stash: &stash, buffer: &buffer, dht: &dht };
        let post = AppPayload::Post { reply_to: None, body: Body::Text("hello".into()) };
        let post = post.ser().unwrap();
        let sealed = seal_application_message(&alice.provider, &mut group, &alice.leaf, &post);
        let bytes = sealed.unwrap().address_to(&bob.ipk, &alice.identity).unwrap();
        let Ok(MlsEnvelopeP::Application(env)) = MlsEnvelopeP::deser(&bytes) else {
            panic!("not an application envelope")
        };
        let id = next_dispatch_id();
        let receive =
            || process_application_inbound_for(&ctx, alice.ipk, &bob.ipk, env.clone(), 0, id);

        let failed = with_failing_trigger(&bob.db, "INSERT ON mls_group_received", receive);
        assert!(crate::utils::is_storage_error(&failed.unwrap_err()));
        let failed = with_failing_trigger(scope.core.db.messages(), "INSERT ON messages", receive);
        assert!(crate::utils::is_storage_error(&failed.unwrap_err()));
        assert!(matches!(receive(), Ok(InboundDecoded::ApplicationUndecryptable)));
        let sql = "SELECT content FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2";
        let key = (conversation.as_slice(), id.as_slice());
        let saved: String =
            scope.core.db.messages().lock().query_row(sql, key, |r| r.get(0)).unwrap();
        assert_eq!(saved, "hello");
    }

    /// Messages that arrive a commit early apply, once it lands, as their MLS author wrote them,
    /// whoever carried them.
    #[tokio::test]
    async fn caught_up_messages_apply_as_their_mls_author_wrote_them() {
        let scope = ScopedCore::new();
        let (alice, bob, carol) = (Party::new(0xA5), Party::new(0xB5), Party::new(0xC5));
        let db = &scope.core.db;
        identity(&db.identity().lock(), 0xB5);
        db.identity().lock().execute("UPDATE identity SET avatar_revision = 9", []).unwrap();
        let gid = [0xD5; 32];
        let meta = GroupMeta::founded("Catch-up".into(), alice.ipk);
        let (mut group, _) = found(&alice, gid, Some(&meta), [&bob, &carol]);
        let members = [alice.ipk, bob.ipk, carol.ipk];
        let conversation = Conversation::join_group(&alice.ipk, &members).unwrap();
        Conversation::bind_group(&conversation, &gid).unwrap();
        let stash = KeyPackageStash::new(bob.db.clone());
        let (buffer, dht) = (EpochCatchupBuffer::new(bob.db.clone()), FakeDhtClient::default());
        let ctx = MlsContext { provider: &bob.provider, stash: &stash, buffer: &buffer, dht: &dht };
        let receive = |sealed: SealedMessage, carrier: &Party, id| {
            let bytes = sealed.address_to(&bob.ipk, &carrier.identity).unwrap();
            let Ok(MlsEnvelopeP::Application(env)) = MlsEnvelopeP::deser(&bytes) else {
                panic!("not an application envelope")
            };
            process_application_inbound_for(&ctx, carrier.ipk, &bob.ipk, env, 123_000, id).unwrap()
        };

        let id = next_dispatch_id;
        let (post, revised, edited, deleted, foreign) = (id(), id(), id(), id(), id());
        for (did, author) in [(revised, alice.ipk), (edited, alice.ipk), (foreign, carol.ipk)] {
            let original = Body::Text("original".into());
            save_inbound_body(&conversation, &author, &did, 1, None, original).unwrap();
        }
        let mine = Message::save_outgoing(conversation, "hello", None).unwrap().inner.dispatch_id;
        let mine: [u8; 16] = mine.unwrap().try_into().unwrap();
        let attachment = |caption: &str, file_id| Body::Attachment {
            caption: caption.into(),
            group_id: None,
            mime: "application/octet-stream".into(),
            name: "f.bin".into(),
            size: 1,
            thumb: vec![],
            file_id,
        };
        let file = [0xF5; 32];
        let mut audience = vec![bob.ipk, carol.ipk];
        audience.sort_unstable();
        let grant = AttachmentSharing {
            message_id: post,
            file_id: file,
            size: 1,
            expires_at: now_secs() + 3600,
            recipients: audience.clone(),
        };
        let (delivered_at, read_at) = (Some(100), Some(101));
        let receipt = ReceiptEntry { message_id: mine, delivered_at, read_at };
        let image = b"\0\0\0\x0cftypavif".to_vec();
        let revision = Body::Text("revised".into());
        // In arrival order, each with its carrier and dispatch id. The grant outruns its post.
        let rows = [
            (AppPayload::AttachmentSharing(grant), &bob, id()),
            (AppPayload::Post { reply_to: None, body: attachment("file", file) }, &alice, post),
            (AppPayload::Revise { target: revised, body: revision }, &carol, id()),
            (AppPayload::Edit { target: edited, content: "edited".into() }, &carol, id()),
            (AppPayload::Delete { target: deleted }, &carol, id()),
            (AppPayload::Delete { target: foreign }, &alice, id()),
            (AppPayload::ReceiptDetails(ReceiptDetails { entries: vec![receipt] }), &carol, id()),
            (AppPayload::Avatar { revision: 10, avif: Some(image.clone()) }, &alice, id()),
            (AppPayload::AvatarSync { known_revision: Some(9), reply: true }, &alice, id()),
            (AppPayload::AvatarAck { revision: 9 }, &alice, id()),
        ];
        let epoch = group.epoch();
        let commit = alice.update(&mut group);
        group.merge_pending_commit(&alice.provider).unwrap();
        for (payload, carrier, id) in rows {
            let bytes = payload.ser().unwrap();
            let sealed = seal_application_message(&alice.provider, &mut group, &alice.leaf, &bytes);
            let outcome = receive(sealed.unwrap(), carrier, id);
            assert!(matches!(outcome, InboundDecoded::ApplicationBuffered));
        }
        assert!(crate::data::media::get(&conversation, &post).unwrap().is_none());
        let before = crate::data::peer_avatar::generation();
        receive(SealedMessage::from_mls_out(&commit, gid, epoch).unwrap(), &alice, id());

        let stored = |did: [u8; 16]| -> (String, Vec<u8>) {
            let sql = "SELECT content, sender_ipk FROM messages \
                       WHERE conversation_id = ?1 AND dispatch_id = ?2";
            let key = (conversation.as_slice(), did.as_slice());
            db.messages().lock().query_row(sql, key, |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        let media = crate::data::media::get(&conversation, &post).unwrap().unwrap();
        assert_eq!(media.file_id, Some(file.to_vec()));
        assert_eq!(stored(post).1, alice.ipk);
        assert!(crate::data::seen::Seen::contains(&alice.ipk, &post));
        let sql = "SELECT author, group_id, recipients FROM attachment_sharing \
                   WHERE conversation_id = ?1 AND message_id = ?2";
        let key = (conversation.as_slice(), post.as_slice());
        let shared: (Vec<u8>, Vec<u8>, Vec<u8>) = db
            .messages()
            .lock()
            .query_row(sql, key, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        let recipients = postcard::to_allocvec(&audience).unwrap();
        assert_eq!(shared, (alice.ipk.to_vec(), gid.to_vec(), recipients));

        let content = [revised, edited, foreign].map(|did| stored(did).0);
        assert_eq!(content, ["revised", "edited", "original"]);
        let late = [0xF6; 32];
        let body = attachment("late", late);
        let saved = save_inbound_body(&conversation, &alice.ipk, &deleted, 1, None, body);
        assert!(saved.unwrap().is_none());
        assert!(crate::data::media::get(&conversation, &deleted).unwrap().is_none());
        let offer = crate::data::media::attachment_offer_tx(&db.messages().lock(), &late);
        assert!(offer.unwrap().is_none());

        let info = crate::data::receipts::info(&conversation, &mine).unwrap();
        let seat = |who: [u8; 32]| {
            let r = info.recipients.iter().find(|r| r.member == who).unwrap();
            (r.status, r.delivered_at, r.read_at)
        };
        assert!(info.complete);
        let read = (4, delivered_at, read_at);
        assert_eq!([seat(alice.ipk), seat(carol.ipk)], [read, (0, None, None)]);

        assert_eq!(crate::data::peer_avatar::get(&alice.ipk), Some(image));
        let sql = "SELECT revision FROM avatar_acks WHERE owner_ipk = ?1 AND peer_ipk = ?2";
        let key = (bob.ipk.as_slice(), alice.ipk.as_slice());
        let acked: Option<u64> = db.messages().lock().query_row(sql, key, |r| r.get(0)).unwrap();
        assert_eq!(acked, Some(9));
        let rung = scope.events.0.lock().iter().any(|(t, g)| t == &["peer_avatars"] && *g > before);
        assert!(rung, "the app hears of the picture once its generation has moved");
    }
}
