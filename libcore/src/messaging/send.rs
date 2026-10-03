use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::client_rel::Wake;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::Body;
use common::proto::mls_wire::MAX_FRAMED_MLS_BYTES;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MlsApplicationEnvelopeP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::envelope_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::utils::now_secs;
use ed25519_dalek::SigningKey;
use log::info;
use log::warn;
use openmls::prelude::tls_codec::Serialize as _;
use parking_lot::Mutex as PlMutex;

use super::body::rebuild_pending_payload;
use super::session::MlsContext;
use super::session::group_for_conversation;
use super::session::leaf_signer_for_group;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::db::outbox::OpType;
use crate::delivery;
use crate::delivery::dispatch_queued;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::mls::types::MlsGroupError;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
use crate::state::core;

/// One MLS ciphertext per logical message, re-signed per recipient because the envelope binds one
/// `to_ipk`. Encrypting per recipient would advance the ratchet N times and strand N-1 generations.
pub struct SealedMessage {
    pub(crate) group_id:  [u8; 32],
    pub(crate) epoch:     u64,
    pub(crate) mls_bytes: Vec<u8>,
    pub(crate) branch:    Option<[u8; 32]>,
    pub(crate) proof:     Option<Vec<u8>>,
}

/// Encrypt `plaintext` to the group once, advancing the ratchet one step.
pub fn seal_application_message(
    provider: &PromtuzMlsProvider, group: &mut MlsGroupHandle,
    leaf_signer: &openmls_basic_credential::SignatureKeyPair, plaintext: &[u8],
) -> Result<SealedMessage, MlsGroupError> {
    let mls_msg = group.create_application_message(provider, leaf_signer, plaintext)?;
    let mls_bytes = mls_msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)?;

    if mls_bytes.len() > MAX_FRAMED_MLS_BYTES {
        return Err(MlsGroupError::Internal(format!(
            "mls_bytes {} exceeds MAX_FRAMED_MLS_BYTES = {}",
            mls_bytes.len(),
            MAX_FRAMED_MLS_BYTES
        )));
    }

    Ok(SealedMessage {
        group_id: group.group_id(),
        epoch: group.epoch(),
        mls_bytes,
        branch: None,
        proof: None,
    })
}

/// Seals `payload` once for `gid` and queues one copy per recipient in the outbox, all under the
/// group's operation lock so no two sends share a ratchet step. A signed group seals on its branch.
pub(crate) fn queue_application(
    outbox: &PlMutex<rusqlite::Connection>, provider: &PromtuzMlsProvider, gid: [u8; 32],
    id: [u8; 16], payload: Vec<u8>, recipients: &[[u8; 32]], signer: &SigningKey, op: OpType,
    wake: Wake, ttl_ms: u64, durable: bool,
) -> Result<Vec<crate::groups::recovery::Copy>> {
    use crate::groups::recovery;
    let _operation = crate::mls::recovery::operation_lock(&gid).lock();
    let mut group =
        MlsGroupHandle::load(provider, &gid)
            .map_err(|e| anyhow!("load group: {e}"))?
            .ok_or_else(|| anyhow!("no local group state for {}", hex::encode(&gid[..4])))?;
    if group.group_meta().is_some_and(|m| m.state.is_some()) {
        return recovery::queue_locked(
            outbox, provider, gid, id, payload, recipients, signer, op, wake, ttl_ms, durable,
        );
    }
    let leaf = leaf_signer_for_group(provider, &group, &signer.verifying_key().to_bytes())?;
    let sealed = seal_application_message(provider, &mut group, &leaf, &payload)?;
    let mut copies =
        recovery::copies(&recovery::addressed(&sealed, id, recipients, signer, op, wake, ttl_ms)?);
    delivery::enqueue_batch_in(&mut outbox.lock(), &mut copies)?;
    Ok(copies)
}

impl SealedMessage {
    /// Commits ride the application envelope. `epoch` must be the epoch the Commit was created in,
    /// before the sender merges it, since recipients still hold that epoch.
    pub fn from_mls_out(
        msg: &openmls::prelude::MlsMessageOut, group_id: [u8; 32], epoch: u64,
    ) -> Result<Self, MlsGroupError> {
        let mls_bytes = msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)?;
        if mls_bytes.len() > MAX_FRAMED_MLS_BYTES {
            return Err(MlsGroupError::Internal(format!(
                "commit {} exceeds MAX_FRAMED_MLS_BYTES = {}",
                mls_bytes.len(),
                MAX_FRAMED_MLS_BYTES
            )));
        }
        Ok(SealedMessage { group_id, epoch, mls_bytes, branch: None, proof: None })
    }

    /// Cheap: a signature over a transcript, with no MLS state touched.
    pub fn address_to(
        &self, to: &[u8; 32], ipk_signer: &SigningKey,
    ) -> Result<Vec<u8>, MlsGroupError> {
        use ed25519_dalek::Signer;
        let transcript = if let Some(branch) = &self.branch {
            common::proto::mls_wire::group_envelope_signing_input(
                PROTOCOL_VERSION,
                to,
                &self.group_id,
                self.epoch,
                branch,
                &self.mls_bytes,
            )
        } else {
            envelope_signing_input(
                PROTOCOL_VERSION,
                to,
                &self.group_id,
                self.epoch,
                &self.mls_bytes,
            )
        };
        let outer_sig = ipk_signer.sign(&transcript);

        let env = MlsApplicationEnvelopeP {
            version:     MLS_ENVELOPE_VERSION,
            group_id:    self.group_id.into(),
            epoch:       self.epoch,
            mls_message: ByteVec(self.mls_bytes.clone()),
            sender_sig:  outer_sig.to_bytes().into(),
        };
        self.branch
            .map_or_else(
                || MlsEnvelopeP::Application(env.clone()),
                |branch| MlsEnvelopeP::GroupApplication {
                    branch:  branch.into(),
                    message: env.clone(),
                    proof:   self.proof.clone().map(Into::into),
                },
            )
            .ser()
            .map_err(|e| MlsGroupError::Internal(format!("postcard ser envelope: {e}")))
    }
}

/// Rebuilds the payload from the stored row, so a retry reuses its dispatch id and the recipient
/// dedups it.
pub async fn attempt_send<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: [u8; 16], msg: Message,
) -> Result<()> {
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_payload(ctx, conversation, &msg, payload_bytes).await
}

/// Encrypts once and unicasts to every member. One durable acceptance marks the message sent;
/// delivered and read need every original recipient.
pub(super) async fn send_payload<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: [u8; 16], msg: &Message, payload_bytes: Vec<u8>,
) -> Result<()> {
    let msg_id = msg.inner.id;
    let current_recipients = Conversation::recipients(&conversation);
    let mut recipients = crate::data::receipts::audience(&msg_id.to_string(), &current_recipients)?;
    if recipients.is_empty() {
        Message::mark_failed(&msg_id);
        MessageEv::Failed {
            id: msg_id,
            conversation,
            reason: "conversation has no members".into(),
        }
        .emit();
        return Err(anyhow!("conversation has no members"));
    }

    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    if !crate::groups::may_post(&conversation, &our_ipk) {
        Message::mark_failed(&msg_id);
        MessageEv::Failed {
            id: msg_id,
            conversation,
            reason: "Only admins can send messages".into(),
        }
        .emit();
        return Err(anyhow!("only admins can send messages in this group"));
    }
    let ipk_signer: SigningKey = crate::data::identity::secret_key_signing(&our_ipk)?;

    let group = match group_for_conversation(ctx, &conversation, &our_ipk, &ipk_signer).await {
        Ok(g) => g,
        Err(e) => {
            if e.downcast_ref::<crate::groups::migration::Pending>().is_some() {
                return Ok(());
            }
            if awaits_keypackage(&e) {
                info!("MESSAGE: recipient has no published KP yet — left pending, will retry");
                return Ok(());
            }
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, conversation, reason: e.to_string() }.emit();
            return Err(e);
        },
    };

    if let Err(e) = leaf_signer_for_group(ctx.provider, &group, &our_ipk) {
        Message::mark_failed(&msg_id);
        MessageEv::Failed { id: msg_id, conversation, reason: e.to_string() }.emit();
        return Err(e);
    }

    let id: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .expect("save_outgoing always mints a dispatch_id")
        .try_into()
        .expect("dispatch_id is 16 bytes");

    let sharing = match AppPayload::deser(&payload_bytes) {
        Ok(AppPayload::Post { body: Body::Attachment { file_id, size, .. }, .. }) => {
            crate::transfer::sharing::for_outgoing(
                &conversation,
                &group.group_id(),
                &our_ipk,
                id,
                file_id,
                size,
                &recipients,
                &group.roster(),
            )?
        },
        _ => None,
    };

    // A legacy session reaches only current members its roster holds; the rest fail at once.
    let signed = group.group_meta().is_some_and(|m| m.state.is_some());
    if !signed {
        let roster = group.roster();
        for to in
            recipients.extract_if(.., |to| !current_recipients.contains(to) || !roster.contains(to))
        {
            crate::data::receipts::send_result(
                &id,
                Some(to),
                crate::data::message::STATUS_FAILED,
                None,
            )?;
        }
    }
    let gid = group.group_id();

    // Queue the whole fan-out before the first network await: once one copy is sent the message is
    // no longer pending, so a restart must find every other copy queued.
    let mut copies = Vec::new();
    if let Some((control_id, offer)) = &sharing {
        let ttl = offer.expires_at.saturating_sub(now_secs()).saturating_mul(1000);
        if ttl > 0 {
            let grantees: Vec<_> =
                offer.recipients.iter().copied().filter(|r| recipients.contains(r)).collect();
            copies.extend(queue_application(
                core().db.outbox(),
                ctx.provider,
                gid,
                *control_id,
                AppPayload::AttachmentSharing(offer.clone()).ser()?,
                &grantees,
                &ipk_signer,
                OpType::Control,
                Wake::No,
                ttl,
                true,
            )?);
        }
    }
    let queued = queue_application(
        core().db.outbox(),
        ctx.provider,
        gid,
        id,
        payload_bytes,
        &recipients,
        &ipk_signer,
        OpType::Message,
        Wake::Message,
        0,
        true,
    );
    // A legacy message its group refuses to seal is failed rather than retried.
    if let Err(e) = &queued
        && !signed
        && e.downcast_ref::<MlsGroupError>().is_some()
    {
        Message::mark_failed(&msg_id);
    }
    copies.extend(queued?);
    for (to, dispatch, op, bytes) in copies {
        if op == OpType::Control {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                dispatch_queued(core().session().as_deref(), &to, &dispatch, op, &bytes),
            )
            .await;
        } else {
            dispatch_queued(core().session().as_deref(), &to, &dispatch, op, &bytes).await;
        }
    }
    Ok(())
}

/// A missing KeyPackage is transient, so the message stays pending for a retry. The fetch fails
/// before any group state exists, so the retry cannot duplicate the group.
fn awaits_keypackage(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<DhtClientError>(), Some(DhtClientError::NoStash)))
}

/// Re-drives pending rows the outbox holds nothing for: a first send deferred on a missing
/// KeyPackage, or one that failed before it was enqueued. Queued rows belong to `reconcile`.
pub async fn retry_pending_sends<C: DhtClient>(ctx: &MlsContext<'_, C>) {
    // Snapshot first: a first send binds its group, which `attempt_send` handles.
    let deferred: Vec<_> = Message::pending_outgoing()
        .into_iter()
        .filter(|row| !row.dispatch_id.as_deref().is_some_and(delivery::any_pending))
        .collect();
    for row in deferred {
        let conversation = row.conversation_id;
        if let Err(e) = attempt_send(ctx, conversation, Message { inner: row }).await {
            warn!(
                "MESSAGE: retry_pending_sends: {} still failing: {e}",
                hex::encode(&conversation[..4])
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use common::proto::client_rel::CRelayPacket;
    use openmls::prelude::MlsMessageIn;
    use openmls::prelude::ProcessedMessageContent;
    use openmls::prelude::tls_codec::Deserialize as _;

    use super::*;
    use crate::messaging::session::lazy_create_group;
    use crate::test_support::net::Device;
    use crate::test_support::net::FakeDhtClient;
    use crate::test_support::net::pair;

    /// Concurrent sends on one pair never share a ratchet step: the peer decrypts every copy in
    /// the order the outbox queued them.
    #[tokio::test]
    async fn concurrent_sends_never_reuse_a_sender_generation() {
        let (alice, bob, dht) = (Device::new(1), Device::new(2), FakeDhtClient::default());
        let (group, mut joined) = pair(&alice, &bob, &dht).await;
        let (gid, to) = (group.group_id(), [bob.ipk]);
        drop(group);
        std::thread::scope(|s| {
            for thread in 0..8u8 {
                let (alice, to) = (&alice, &to);
                s.spawn(move || {
                    for n in thread * 4..thread * 4 + 4 {
                        queue_application(
                            alice.db.outbox(),
                            &alice.provider,
                            gid,
                            [n; 16],
                            vec![n],
                            to,
                            &alice.signer,
                            OpType::Message,
                            Wake::Message,
                            0,
                            true,
                        )
                        .unwrap();
                    }
                });
            }
        });
        let frames: Vec<Vec<u8>> = {
            let conn = alice.db.outbox().lock();
            let mut rows = conn.prepare("SELECT payload FROM outbox ORDER BY rowid").unwrap();
            rows.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let mut received: Vec<u8> = frames
            .iter()
            .map(|frame| {
                let Ok(CRelayPacket::Dispatch(dispatch)) = CRelayPacket::deser(&frame[4..]) else {
                    panic!("not a dispatch")
                };
                let Ok(MlsEnvelopeP::Application(env)) = MlsEnvelopeP::deser(&dispatch.payload)
                else {
                    panic!("not an application envelope")
                };
                let message = MlsMessageIn::tls_deserialize_exact(&env.mls_message.0)
                    .unwrap()
                    .try_into_protocol_message()
                    .unwrap();
                match joined.process_incoming(&bob.provider, message).unwrap().content {
                    ProcessedMessageContent::ApplicationMessage(m) => m.into_bytes()[0],
                    _ => panic!("not an application message"),
                }
            })
            .collect();
        received.sort_unstable();
        assert_eq!(received, (0..32).collect::<Vec<u8>>());
    }

    /// A first send to someone with no published KeyPackage stays pending for a retry; any other
    /// failure to start the pair fails the message.
    #[tokio::test]
    async fn a_first_send_waits_for_a_keypackage_but_other_setup_errors_fail() {
        let (alice, bob, dht) = (Device::new(5), Device::new(6), FakeDhtClient::default());
        let ctx = alice.ctx(&dht);
        let start = || lazy_create_group(&ctx, &alice.ipk, &alice.signer, &bob.ipk);
        assert!(awaits_keypackage(&start().await.unwrap_err()));

        *dht.fetch_error.lock() = Some(DhtClientError::Transport("unreachable".into()));
        assert!(!awaits_keypackage(&start().await.unwrap_err()));

        let mut record = bob.stash.generate_one(&bob.provider, &bob.signer).unwrap();
        record.owner_sig.0[0] ^= 1;
        dht.publish_keypackages(&[record]).await.unwrap();
        assert!(!awaits_keypackage(&start().await.unwrap_err()));
    }
}
