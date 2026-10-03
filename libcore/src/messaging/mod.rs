//! End-to-end encrypted application messaging via MLS.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use common::proto::client_rel::Wake;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::Body;
use common::proto::pack::Packer;
use common::utils::now_secs;
use log::debug;

use self::body::rebuild_pending_payload;
use self::body::stored_body;
use self::send::queue_application;
use self::send::send_payload;
use self::session::MlsContext;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::data::reaction::Reaction;
use crate::db::outbox::OpType;
use crate::delivery;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::events::messaging::ReactionEv;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::PromtuzMlsProvider;
use crate::state::core;

pub(crate) mod body;
pub mod receive;
pub mod send;
pub mod session;
pub mod welcome;

#[derive(Default)]
pub(crate) struct Messaging {
    /// One `lazy_create_group` at a time per scope: concurrent first sends would each burn a
    /// KeyPackage and create a duplicate group. Entries are never removed.
    group_create:      parking_lot::Mutex<HashMap<Vec<u8>, Arc<tokio::sync::Mutex<()>>>>,
    failed_dispatches: parking_lot::Mutex<HashMap<([u8; 32], [u8; 16]), u8>>,
    /// Per-`welcome_id` failure counts, in memory only: a restart resets them, and the home's TTL
    /// still bounds the queue.
    welcome_retries:   parking_lot::Mutex<HashMap<[u8; 8], u8>>,
}

/// `msg` is already saved, so a failure leaves it pending rather than lost; without a relay it
/// waits for `retry_pending_sends`.
pub(crate) async fn send_prepared(
    conversation: [u8; 16], msg: &Message, payload_bytes: Vec<u8>,
) -> Result<()> {
    let dht_client = core().session().map(|s| s.dht.clone());
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(core().db.mls());
    let buffer = EpochCatchupBuffer::new(core().db.mls());
    match dht_client {
        Some(client) => {
            let ctx = MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      client.as_ref(),
            };
            send_payload(&ctx, conversation, msg, payload_bytes).await
        },
        None => {
            debug!("MESSAGE: no relay connection; message stays pending");
            Ok(())
        },
    }
}

/// Editing the text field changes a caption without changing its media body.
pub(crate) fn text_edit_body(
    conversation: &[u8; 16], target: &[u8; 16], text: String,
) -> Result<Body> {
    let msg = Message::get_by_dispatch(conversation, target)
        .ok_or_else(|| anyhow!("message not found"))?;
    let mut body = stored_body(conversation, &msg)?;
    match &mut body {
        Body::Text(content) => *content = text,
        Body::Image { caption, .. } | Body::Attachment { caption, .. } => *caption = text,
        _ => bail!("this message has no editable text"),
    }
    Ok(body)
}

/// `for_everyone` tombstones it on both sides; otherwise it is removed locally with no wire signal.
pub async fn delete(conversation: [u8; 16], target: [u8; 16], for_everyone: bool) -> Result<()> {
    {
        let provider = PromtuzMlsProvider::shared();
        if let Some(gid) = Conversation::group_of(&conversation) {
            let _operation = crate::mls::recovery::operation_lock(&gid).lock();
            crate::mls::recovery::discard_message_replay(&provider, &gid, &target)?;
        }
    }
    // Never sent: nothing to tombstone, and a pending tombstone would spin forever since resends
    // skip deleted rows. The Delete still ships in case the send raced us.
    let never_sent = Message::get_by_dispatch(&conversation, &target)
        .is_some_and(|m| matches!(m.inner.status, 0 | 2));
    let row = if for_everyone && !never_sent {
        // own=true: delete-for-everyone only tombstones our own sent messages.
        Message::apply_delete(&conversation, &target, true, None)
    } else {
        // Delete-for-me is a local-only hide; any message in our view is fair game.
        Message::hard_delete(&conversation, &target)
    };
    if let Some(row) = row {
        MessageEv::Deleted { id: row.id, conversation }.emit();
    }
    if for_everyone {
        send_control(conversation, AppPayload::Delete { target }).await
    } else {
        Ok(())
    }
}

/// Applied locally first so the UI reflects it at once; best-effort on the wire.
pub async fn react(
    conversation: [u8; 16], target: [u8; 16], emoji: String, add: bool,
) -> Result<()> {
    ensure!(!crate::requests::is_request_chat(&conversation), "Accept the request first");
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    let ts = now_secs();
    if Reaction::apply(&conversation, &target, &our_ipk, &emoji, add, ts) {
        ReactionEv {
            conversation,
            dispatch_id: target,
            reactor: our_ipk,
            emoji: emoji.clone(),
            add,
        }
        .emit();
    }
    send_control(conversation, AppPayload::React { target, emoji, add }).await
}

/// An MLS application message into the conversation's existing group. Durable controls are
/// outboxed and replayed on reconnect.
pub(crate) async fn send_control(conversation: [u8; 16], payload: AppPayload) -> Result<()> {
    send_control_inner(conversation, payload, Wake::No, None).await
}

/// [`send_control`] with an explicit wake class, for call signaling: an
/// offer rings a sleeping phone, the rest of a call must not.
pub(crate) async fn send_control_class(
    conversation: [u8; 16], payload: AppPayload, wake: Wake,
) -> Result<()> {
    send_control_inner(conversation, payload, wake, None).await
}

/// [`send_control`] to one member, for state only they are missing, like the group's name after
/// they join. MLS tolerates the generation gap this leaves in the others' view of our ratchet.
pub(crate) async fn send_control_to(
    conversation: [u8; 16], payload: AppPayload, to: [u8; 32],
) -> Result<()> {
    send_control_inner(conversation, payload, Wake::No, Some(to)).await
}

/// [`send_control`] to one member with a push wake, so an offline peer is revived.
pub(crate) async fn send_control_wake_to(
    conversation: [u8; 16], payload: AppPayload, to: [u8; 32],
) -> Result<()> {
    send_control_inner(conversation, payload, Wake::Message, Some(to)).await
}

/// A control send owns its temporary outbox copies even while an ACK is
/// pending. Cancellation must not turn an expired offer into a durable retry.
struct ControlDispatchGuard {
    id: [u8; 16],
    ephemeral: bool,
}

impl Drop for ControlDispatchGuard {
    fn drop(&mut self) {
        if self.ephemeral {
            delivery::retire_all(&self.id);
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("control probe deferred until a recipient's relay accepts it")]
pub(crate) struct ControlDeferred;

async fn send_control_inner(
    conversation: [u8; 16], payload: AppPayload, wake: Wake, only: Option<[u8; 32]>,
) -> Result<()> {
    // Offers, call signaling and profile probes describe current state, so they are never retried,
    // and offers and calls carry a TTL so the relay drops them once stale.
    let ttl_ms = match &payload {
        AppPayload::P2pOffer { .. } => crate::p2p::OFFER_TTL_MS,
        AppPayload::Call(_) => crate::call::SIGNAL_TTL_MS,
        _ => 0,
    };
    let durable = ttl_ms == 0
        && !matches!(
            payload,
            AppPayload::AvatarSync { .. } | AppPayload::ProfileDetailsSync { .. }
        );
    ensure!(!crate::requests::is_request_chat(&conversation), "request not accepted");
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    let mut recipients = Conversation::recipients(&conversation);
    if let Some(one) = only {
        recipients.retain(|r| *r == one);
        if recipients.is_empty() {
            bail!("{} is not in this conversation", hex::encode(&one[..4]));
        }
    }
    if recipients.is_empty() {
        bail!("conversation {} has no members", hex::encode(&conversation[..4]));
    }
    let gid = Conversation::group_of(&conversation)
        .ok_or_else(|| anyhow!("no group for conversation {}", hex::encode(&conversation[..4])))?;
    let id = crate::data::message::next_dispatch_id();
    // Storage failures return to the caller: a receipt ledger must not clear pending work that
    // never reached the outbox.
    let copies = queue_application(
        core().db.outbox(),
        &PromtuzMlsProvider::shared(),
        gid,
        id,
        payload.ser()?,
        &recipients,
        &ipk_signer,
        OpType::Control,
        wake,
        ttl_ms,
        durable,
    )?;
    // A signed group's copies carry a branch-derived wire id.
    let _dispatch =
        ControlDispatchGuard { id: copies.first().map_or(id, |c| c.1), ephemeral: !durable };
    // Queued for every member: fine when durable, a failure when not.
    if crate::groups::recovery::dispatch(copies).await == 0 && !durable {
        return Err(ControlDeferred.into());
    }
    Ok(())
}

/// A placeholder row with a null blob so the bubble shows at once; [`finish_image`] adds bytes.
pub(crate) fn build_image_message(
    conversation: [u8; 16], width: u32, height: u32, caption: &str, group_id: Option<[u8; 16]>,
) -> Result<Message> {
    crate::data::media::save_outgoing_with_media(
        &conversation,
        caption,
        None,
        &crate::data::media::MediaRow {
            kind: crate::data::media::KIND_IMAGE,
            group_id: group_id.map(|g| g.to_vec()),
            mime: "image/avif".into(),
            name: String::new(),
            size: 0,
            width,
            height,
            blob: None,
            thumb: None,
            file_id: None,
            duration_ms: 0,
            sticker: None,
        },
    )
}

/// A placeholder row without a file id; [`finish_attachment`] lands it after the manifest pass.
pub(crate) fn build_attachment_message(
    conversation: [u8; 16], size: u64, name: &str, mime: &str, thumb: Option<Vec<u8>>,
    caption: &str, group_id: Option<[u8; 16]>,
) -> Result<Message> {
    crate::data::media::save_outgoing_with_media(
        &conversation,
        caption,
        None,
        &crate::data::media::MediaRow {
            kind: crate::data::media::KIND_ATTACHMENT,
            group_id: group_id.map(|g| g.to_vec()),
            mime: mime.to_string(),
            name: name.to_string(),
            size,
            width: 0,
            height: 0,
            blob: None,
            thumb,
            file_id: None,
            duration_ms: 0,
            sticker: None,
        },
    )
}

pub(crate) async fn finish_image(
    conversation: [u8; 16], did: [u8; 16], avif: Vec<u8>, width: u32, height: u32,
) -> Result<()> {
    crate::data::media::set_blob(&conversation, &did, &avif, width, height)?;
    let msg = Message::get_by_dispatch(&conversation, &did)
        .ok_or_else(|| anyhow!("image row vanished"))?;
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_prepared(conversation, &msg, payload_bytes).await
}

pub(crate) async fn finish_attachment(
    conversation: [u8; 16], did: [u8; 16], file_id: [u8; 32],
) -> Result<()> {
    crate::data::media::set_file_id(&conversation, &did, &file_id)?;
    let msg = Message::get_by_dispatch(&conversation, &did)
        .ok_or_else(|| anyhow!("attachment row vanished"))?;
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_prepared(conversation, &msg, payload_bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::next_dispatch_id;
    use crate::test_support::ScopedCore;

    /// An abandoned call or P2P offer must not ring later: cancelling its send retires every queued
    /// copy, while a durable control keeps its copies for the reconciler.
    #[tokio::test]
    async fn cancelled_control_retires_only_ephemeral_fanout_and_ack_bookkeeping() {
        let scope = ScopedCore::new();
        for ephemeral in [true, false] {
            let id = next_dispatch_id();
            let (ready, queued) = tokio::sync::oneshot::channel();
            let send = tokio::spawn(async move {
                let _dispatch = ControlDispatchGuard { id, ephemeral };
                for peer in [[0xf1; 32], [0xf2; 32], [0xf3; 32]] {
                    delivery::enqueue(&id, OpType::Control, Some(peer), b"pending control");
                }
                // One member acknowledged; two copies still await theirs.
                delivery::retire(&id, Some([0xf1; 32]));
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            queued.await.unwrap();
            let copies = || {
                let due = delivery::due_tx(&scope.core.db.outbox().lock(), u64::MAX).unwrap();
                due.iter().filter(|row| row.id == id).count()
            };
            assert_eq!(copies(), 2);
            send.abort();
            assert!(send.await.unwrap_err().is_cancelled());
            assert_eq!(copies(), if ephemeral { 0 } else { 2 }, "ephemeral: {ephemeral}");
        }
    }
}
