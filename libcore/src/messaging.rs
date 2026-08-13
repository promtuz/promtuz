//! End-to-end encrypted application messaging via MLS.
//!
//! # Migration from the v2 shared-key scheme
//!
//! Under the old scheme (`PROTOCOL_VERSION = 2`): every contact carried a
//! `(epk, enc_esk)` pair; outgoing messages were encrypted with the
//! deterministic `Contact::shared_key()` and decrypted by the recipient
//! the same way. The relay's view of `DispatchP::payload` was
//! `nonce(12) || ChaCha20Poly1305(content, shared_key, AD=to)`.
//!
//! Under the current MLS scheme (`PROTOCOL_VERSION = 3`): every contact
//! carries a nullable `mls_group_id`; on first dispatch we lazy-create
//! the implicit 1:1 MLS group via the X3DH-equivalent flow:
//!
//! 1. Fetch the recipient's KeyPackage via [`DhtClient::fetch_keypackage_for`].
//! 2. Build a fresh group with [`MlsGroupHandle::create`].
//! 3. Add the recipient via [`MlsGroupHandle::add_members`] → `(commit, welcome)`.
//! 4. Wrap the welcome via [`make_welcome_envelope`] and publish to the recipient's K-closest homes
//!    via [`DhtClient::publish_welcome_to_homes`].
//! 5. Merge the founder's own commit; persist the new group_id back onto the contact row.
//!
//! After lazy-creation (or if the group already exists), application
//! messaging is straightforward:
//!
//! 1. [`MlsGroupHandle::create_application_message`] yields an `MlsMessageOut`.
//! 2. The bytes are wrapped into [`MlsApplicationEnvelopeP`] with an outer
//!    [`envelope_signing_input`] sig under the sender's IPK (outer sig binds `to_ipk` so a
//!    malicious relay can't redirect).
//! 3. The envelope ships in `DispatchP::payload` over the existing sticky-home queue.
//!
//! # Receive path
//!
//! `quic/server.rs::handle_deliver` decodes the envelope, dispatches
//! on variant:
//!
//! - **Welcome**: verify outer sig → [`process_welcome`] → persist group → bind contact's
//!   `mls_group_id` → mark consumed KP via [`KeyPackageStash::on_consumed`].
//! - **Application**: verify outer sig → load group → [`MlsGroupHandle::process_incoming`] (or
//!   buffer in the epoch-ahead queue if `epoch > current`).
//!
//! # Welcome poll on reconnect
//!
//! [`poll_welcomes`] runs once per QUIC reconnect: fetches own
//! pending welcomes from the K-closest homes, processes each
//! (verifying the outer sig under the inviter's IPK), persists the
//! new group, then acks via [`DhtClient::ack_welcomes`] so the
//! homes can GC.
//!
//! # Module organisation
//!
//! - [`send_message_inner`] — entry point for outgoing messages (called by the JNI `sendMessage`).
//! - [`process_inbound_envelope`] — dispatcher for inbound MLS envelopes; called from
//!   `quic/server.rs::handle_deliver`.
//! - [`poll_welcomes`] — drain pending welcomes on reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::PROTOCOL_VERSION;
use common::proto::client_rel::ActivityP;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::SubscribePresenceP;
use common::proto::client_rel::activity_sig_message;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::Body;
use common::proto::mls_wire::MAX_FRAMED_MLS_BYTES;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MlsApplicationEnvelopeP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::PairDeclineP;
use common::proto::mls_wire::PairingP;
use common::proto::mls_wire::ReceiptKind;
use common::proto::mls_wire::SystemEvent;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::envelope_signing_input;
use common::proto::mls_wire::pair_decline_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use log::debug;
use log::error;
use log::info;
use log::warn;
use once_cell::sync::Lazy;
use openmls::prelude::BasicCredential;
use openmls::prelude::CredentialWithKey;
use openmls::prelude::KeyPackage;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::ProtocolMessage;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use parking_lot::Mutex as PlMutex;
use tokio::sync::Mutex as TokMutex;

use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::data::reaction::Reaction;
use crate::db::mls::stash_db_handle;
use crate::db::outbox::OpType;
use crate::delivery;
use crate::delivery::LastOutcome;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::events::messaging::ReactionEv;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PROMTUZ_CIPHERSUITE;
use crate::mls::PromtuzMlsProvider;
use crate::mls::make_welcome_envelope;
use crate::mls::process_welcome;
use crate::mls::types::MlsGroupError;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
// RELAY lives in `crate::state`; the `quic::server` re-export is kept
// for backwards compatibility but the cycle-breaking import points at
// the leaf module directly.
use crate::state::RELAY;

/// Per-recipient guard around `lazy_create_group`.
///
/// Two simultaneous first-sends to the same contact would otherwise
/// each (a) fetch + burn a recipient KP, (b) create a duplicate
/// orphan group, and (c) double-count against the recipient's KP stash.
/// The guard is
/// keyed on the recipient's IPK; the outer `parking_lot::Mutex`
/// gates the keyed-map insert (no `await` held), and the inner
/// `tokio::sync::Mutex` is what `lazy_create_group` actually
/// awaits while it does the network round trips.
///
/// **Memory**: entries are not GC'd here — they're tiny `Arc<TokMutex<()>>`
/// pairs, and the keyspace is "contacts you've messaged this session".
/// A dedicated cleanup task is overkill given typical contact-graph
/// sizes (<10^4); revisit if memory profile shows it matters.
#[allow(clippy::type_complexity)] // The shape is the API; type alias adds a hop without clarity.
static GROUP_CREATE_LOCKS: Lazy<PlMutex<HashMap<Vec<u8>, Arc<TokMutex<()>>>>> =
    Lazy::new(|| PlMutex::new(HashMap::new()));

/// Acquire (or create) the `lazy_create_group` lock for one scope — a
/// conversation id on the send path, a peer IPK on the inbound heal path.
fn group_create_lock(scope: &[u8]) -> Arc<TokMutex<()>> {
    let mut map = GROUP_CREATE_LOCKS.lock();
    map.entry(scope.to_vec()).or_insert_with(|| Arc::new(TokMutex::new(()))).clone()
}

#[cfg(not(test))]
const _: () = ();

/// Mint a fresh 32-byte `group_id`:
/// `BLAKE3("promtuz-mls-v1 group-id" || creator_ipk || created_at_be_u64 || random_32B)`.
///
/// Random suffix is from `OsRng` (via the same dalek-re-exported
/// `rand_core` we use elsewhere in libcore — keeps us off the
/// multi-version `rand_core` workspace minefield). Returns the
/// 32-byte digest. Unique-by-construction because the random suffix
/// is sender-private; no other party can independently mint the same id.
pub(crate) fn mint_group_id(creator_ipk: &[u8; 32]) -> [u8; 32] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;

    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    let now_ms = crate::utils::systime().as_millis() as u64;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"promtuz-mls-v1 group-id");
    hasher.update(creator_ipk);
    hasher.update(&now_ms.to_be_bytes());
    hasher.update(&nonce);
    *hasher.finalize().as_bytes()
}

/// Decode the recipient's fetched KeyPackage bytes into an openmls
/// `KeyPackage`. Validates the cipher suite is exactly `0x0003`
/// (`PROMTUZ_CIPHERSUITE`); other suites are spec-incompatible.
fn decode_keypackage_bytes(kp_bytes: &[u8]) -> Result<KeyPackage, MlsGroupError> {
    use openmls::prelude::KeyPackageIn;
    use openmls::prelude::ProtocolVersion;
    let kp_in = KeyPackageIn::tls_deserialize_exact(kp_bytes).map_err(MlsGroupError::from_codec)?;
    let kp = kp_in
        .validate(
            // We use openmls's `RustCrypto` provider — provided here via
            // the workspace dep so we don't have to reach into
            // `PromtuzMlsProvider`'s `crypto()` and risk circular
            // dependency in tests.
            &openmls_rust_crypto::RustCrypto::default(),
            ProtocolVersion::Mls10,
        )
        .map_err(|e| MlsGroupError::Internal(format!("KeyPackageIn::validate: {e:?}")))?;
    if kp.ciphersuite() != PROMTUZ_CIPHERSUITE {
        return Err(MlsGroupError::BadCipherSuite);
    }
    Ok(kp)
}

/// Build a credential-with-key bundle for the founder's leaf, using a
/// **fresh** Ed25519 leaf signing key (distinct from the IPK so a leaf
/// compromise can't recover IPK_priv).
///
/// Returns `(SignatureKeyPair, CredentialWithKey)`. The caller persists
/// the signing keypair in openmls's storage via `.store(provider.storage())`
/// before invoking `MlsGroup::new_with_group_id`.
pub(crate) fn build_self_credential(
    own_ipk: &[u8; 32],
) -> Result<(openmls_basic_credential::SignatureKeyPair, CredentialWithKey), MlsGroupError> {
    let credential = BasicCredential::new(own_ipk.to_vec());
    let leaf_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
        .map_err(|e| MlsGroupError::Internal(format!("leaf signature key: {e:?}")))?;
    let cwk = CredentialWithKey {
        credential:    credential.into(),
        signature_key: leaf_kp.public().into(),
    };
    Ok((leaf_kp, cwk))
}

/// Send `content` to `to`. Builds the production MLS context from the
/// live relay's peer/5 dialer and hands off to [`send_message_inner`].
/// Errors (rather than silently no-op'ing against a non-wired dialer) if
/// no relay connection is up. Entry point for `api::messaging::send_message`.
pub async fn send(
    conversation: [u8; 16], content: String, reply_to: Option<[u8; 16]>,
) -> Result<()> {
    // Pull the production peer/5 dialer from the global `RELAY` (attached
    // at connect-time by `Relay::connect`). If no relay connection is
    // live, surface a clean error rather than silently no-op against
    // `NotWiredDhtClient` — otherwise every send would claim success while
    // no Welcome reached the wire.
    let dht_client = {
        let guard = RELAY.read();
        guard.as_ref().and_then(|r| r.dht_client.clone())
    };
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(stash_db_handle());
    let buffer = EpochCatchupBuffer::new(stash_db_handle());
    match dht_client {
        Some(client) => {
            let ctx = MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      client.as_ref(),
            };
            send_message_inner(&ctx, conversation, content, reply_to).await
        },
        None => {
            error!("MESSAGE: no relay connection / dialer; send aborted");
            Err(anyhow!("not connected to a relay (peer/5 dialer not wired); reconnect first"))
        },
    }
}

/// Send an already-persisted `msg` with an already-serialized payload: build
/// the production `MlsContext` (same dance as [`send`]) and drive
/// [`send_payload`]. Tail of the optimistic-media path — the row landed
/// before the heavy prep, so on failure it stays pending rather than lost.
pub(crate) async fn send_prepared(
    conversation: [u8; 16], msg: &Message, payload_bytes: Vec<u8>,
) -> Result<()> {
    let dht_client = {
        let guard = RELAY.read();
        guard.as_ref().and_then(|r| r.dht_client.clone())
    };
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(stash_db_handle());
    let buffer = EpochCatchupBuffer::new(stash_db_handle());
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
            error!("MESSAGE: no relay connection / dialer; media send aborted");
            Err(anyhow!("not connected to a relay (peer/5 dialer not wired); reconnect first"))
        },
    }
}

/// Edit a prior message to `to` and apply the change locally. Best-effort on
/// the wire — the relay queues it for an offline peer; a mid-send failure while
/// WE are offline leaves the local edit applied but unpropagated (MVP).
pub async fn edit(conversation: [u8; 16], target: [u8; 16], content: String) -> Result<()> {
    revise(conversation, target, Body::Text(content)).await
}

/// Replace a prior message's body. Applies locally first — which is also where
/// the compatibility matrix is enforced, so a refused swap never reaches the
/// wire — then ships the same body to the peer. `own=true` throughout: we only
/// revise our own sent messages (outgoing=1).
pub async fn revise(conversation: [u8; 16], target: [u8; 16], body: Body) -> Result<()> {
    if let Some((row, content)) = apply_revise_body(&conversation, &target, body.clone(), true)? {
        MessageEv::Edited { id: row.id, conversation, content }.emit();
    }
    send_control(conversation, AppPayload::Revise { target, body }).await
}

/// Delete a prior message. `for_everyone` tombstones both sides (sends a
/// Delete); otherwise it's a local-only removal, no wire signal.
pub async fn delete(conversation: [u8; 16], target: [u8; 16], for_everyone: bool) -> Result<()> {
    let row = if for_everyone {
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

/// Add or remove our own emoji reaction on a prior message, then propagate it.
/// Applied locally first (reactor = us) so the UI reflects it instantly;
/// best-effort on the wire like edit/delete.
pub async fn react(
    conversation: [u8; 16], target: [u8; 16], emoji: String, add: bool,
) -> Result<()> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ts = crate::utils::systime().as_secs();
    if Reaction::apply(&conversation, &target, &our_ipk, &emoji, add, ts) {
        ReactionEv { conversation, dispatch_id: target, reactor: our_ipk, emoji: emoji.clone(), add }
            .emit();
    }
    send_control(conversation, AppPayload::React { target, emoji, add }).await
}

/// Send a read/delivered receipt: tell `to` we've received-or-read their
/// messages up to `upto` (a 16-byte dispatch_id). High-water-mark — one
/// receipt supersedes earlier ones. Best-effort, like the other control sends.
pub async fn send_receipt(
    conversation: [u8; 16], kind: ReceiptKind, upto: [u8; 16],
) -> Result<()> {
    send_control(conversation, AppPayload::Receipt { kind, upto }).await
}

/// Narrate a membership or title change: store it locally so we see it
/// immediately, then ship it to every member so it orders inline with the
/// conversation on their side too.
///
/// Best-effort on the wire, like the other control sends — a group whose
/// membership changed is still correct if one member's "X was added" line
/// never lands; the Commit is what actually moves them.
pub(crate) async fn announce(conversation: [u8; 16], event: SystemEvent) {
    use crate::db::messages::SYSTEM_ADDED;
    use crate::db::messages::SYSTEM_LEFT;
    use crate::db::messages::SYSTEM_REMOVED;
    use crate::db::messages::SYSTEM_TITLED;

    let Some(our_ipk) = Identity::get().map(|i| i.ipk()) else { return };
    let (code, target) = match &event {
        SystemEvent::Added { who } => (SYSTEM_ADDED, hex::encode(who.0)),
        SystemEvent::Left { who } => (SYSTEM_LEFT, hex::encode(who.0)),
        SystemEvent::Removed { who } => (SYSTEM_REMOVED, hex::encode(who.0)),
        SystemEvent::Titled { title } => (SYSTEM_TITLED, title.clone()),
    };
    let did = crate::data::message::next_dispatch_id();
    let ts = crate::utils::systime().as_secs();
    match Message::save_system(conversation, our_ipk, &did, code, &target, ts, true) {
        Ok(Some(row)) => MessageEv::Received {
            id: row.inner.id,
            conversation,
            sender: our_ipk,
            content: target,
            timestamp: ts,
        }
        .emit(),
        Ok(None) => {},
        Err(e) => warn!("GROUP: could not record a system row: {e}"),
    }
    if let Err(e) = send_control(conversation, AppPayload::System(event)).await {
        warn!("GROUP: system event not delivered: {e}");
    }
}

/// Proof-of-pair: the invitee's first app message after accepting
/// a Welcome. Flips the inviter's contact PENDING → PAIRED by simply being a
/// decryptable inbound message.
pub async fn send_pair_ack(to: [u8; 32]) -> Result<()> {
    let conversation = Conversation::for_peer(&to)?;
    send_control(conversation, AppPayload::PairAck).await
}

/// Send a control `AppPayload` (Edit/Delete/React/Receipt) into the existing 1:1 group as an
/// MLS application message. The group must already exist — you're mutating a
/// message you already exchanged. Outboxed, so a send that finds us offline is
/// replayed on reconnect rather than lost.
pub(crate) async fn send_control(conversation: [u8; 16], payload: AppPayload) -> Result<()> {
    send_control_inner(conversation, payload, false).await
}

/// Reverse-wake variant of [`send_control`]: the same row-less MLS control send,
/// but flags the `DispatchP` for push-wake so an offline peer is revived. The
/// attachment reverse-wake uses it to bring an offline sender back online.
pub(crate) async fn send_control_wake(conversation: [u8; 16], payload: AppPayload) -> Result<()> {
    send_control_inner(conversation, payload, true).await
}

async fn send_control_inner(
    conversation: [u8; 16], payload: AppPayload, wake: bool,
) -> Result<()> {
    // P2P candidates describe a path that is gone by the next reconnect; every
    // other control payload is a state edit every member must eventually see.
    let outbox = (!matches!(payload, AppPayload::P2p { .. })).then_some(OpType::Control);
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    let recipients = Conversation::recipients(&conversation);
    if recipients.is_empty() {
        bail!("conversation {} has no members", hex::encode(&conversation[..4]));
    }
    let gid = Conversation::group_of(&conversation)
        .ok_or_else(|| anyhow!("no group for conversation {}", hex::encode(&conversation[..4])))?;

    let provider = PromtuzMlsProvider::shared();
    let mut group = MlsGroupHandle::load(&provider, &gid)
        .map_err(|e| anyhow!("load group: {e}"))?
        .ok_or_else(|| anyhow!("no local group state for {}", hex::encode(&gid[..4])))?;
    let leaf_kp = leaf_signer_for_group(&provider, &group, &our_ipk)?;

    let payload_bytes = payload.ser().map_err(|e| anyhow!("encode AppPayload: {e}"))?;

    // seal_application_message only touches ctx.provider; a stub dht is fine.
    let stash = KeyPackageStash::new(stash_db_handle());
    let buffer = EpochCatchupBuffer::new(stash_db_handle());
    let dht = crate::quic::dht_client::NotWiredDhtClient;
    let ctx = MlsContext { provider: &provider, stash: &stash, buffer: &buffer, dht: &dht };

    // One seal, one ratchet step, N addressed copies — same discipline as the
    // message path, and for the same reason.
    let sealed = seal_application_message(&ctx, &mut group, &leaf_kp, &payload_bytes)
        .map_err(|e| anyhow!("seal control: {e}"))?;

    let id = crate::data::message::next_dispatch_id();
    let mut delivered = 0usize;
    for to in &recipients {
        let env = sealed
            .address_to(to, &ipk_signer)
            .map_err(|e| anyhow!("address control envelope: {e}"))?;
        let outcome = dispatch_to_member(
            to,
            &our_ipk,
            &ipk_signer,
            &id,
            env,
            outbox.unwrap_or(OpType::Control),
            wake,
        )
        .await;
        if matches!(outcome, LastOutcome::Durable) {
            delivered += 1;
        }
    }
    // A control op with no outbox dies with the attempt; drop any row it left.
    if outbox.is_none() {
        delivery::retire_all(&id);
    }
    if delivered == 0 && !recipients.is_empty() {
        // Every member is queued rather than delivered — fine when outboxed,
        // a real failure when not.
        if outbox.is_none() {
            bail!("control send reached no member");
        }
    }
    Ok(())
}

/// Sign a `DispatchP` over `env_bytes` and send it (the relay queues it for an
/// offline peer). Shared tail of the MLS control path and the non-MLS
/// PairDecline — both just need an authenticated dispatch of opaque bytes.
/// `wake` sets the push-wake flag: false for chat-side control, true for the
/// reverse-wake that must revive an offline sender.
///
/// `outbox` names the op type to persist the framed bytes under: the row
/// outlives a failed attempt and the reconciler re-sends it on the next
/// reconnect, retiring only on a durable ack. `None` sends once and lets the
/// payload die with the attempt.
async fn dispatch_envelope(
    to: [u8; 32], our_ipk: [u8; 32], ipk_signer: &SigningKey, env_bytes: Vec<u8>, wake: bool,
    outbox: Option<OpType>,
) -> Result<()> {
    let id = crate::data::message::next_dispatch_id();
    let sig_message = dispatch_sig_message(&to, &our_ipk, &id, &env_bytes);
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&sig_message).to_bytes()
    };
    let fwd = DispatchP {
        to:             Bytes(to),
        from:           Bytes(our_ipk),
        id:             Bytes(id),
        payload:        ByteVec(env_bytes),
        sig:            Bytes(sig),
        accepted_at_ms: 0,
        wake,
    };
    let bytes = CRelayPacket::Dispatch(fwd).pack().map_err(|e| anyhow!("pack dispatch: {e}"))?;
    if let Some(op) = outbox {
        delivery::enqueue(&id, op, Some(to), &bytes);
    }

    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else {
        info!("MESSAGE: offline — dispatch to {} not sent", hex::encode(&to[..4]));
        bail!("offline");
    };
    let (mut tx, mut rx) =
        conn.open_bi().await.map_err(|e| anyhow!("open dispatch stream: {e}"))?;
    tx.write_all(&bytes).await.map_err(|e| anyhow!("write dispatch: {e}"))?;
    tx.finish().map_err(|e| anyhow!("finish dispatch: {e}"))?;
    let ack = match SRelayPacket::unpack(&mut rx).await {
        Ok(SRelayPacket::DispatchAck(ack)) => ack,
        Ok(other) => bail!("unexpected dispatch reply: {other:?}"),
        Err(e) => bail!("dispatch ack: {e}"),
    };
    if delivery::outcome_for_ack(&ack) != LastOutcome::Durable {
        bail!("relay did not accept dispatch: {ack:?}");
    }
    if outbox.is_some() {
        delivery::retire(&id, Some(to));
    }
    Ok(())
}

/// Tell `to` we declined their pairing Welcome. A plain signed
/// control message on the dispatch/queue channel — no MLS group (accepting it
/// is what failed). The inviter verifies the signature under our IPK, marks us
/// REJECTED, and fails the messages it sent us while PENDING.
pub async fn send_pair_decline(to: [u8; 32], reason: u8) -> Result<()> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;
    let ts = crate::utils::systime().as_millis() as u64;
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&pair_decline_signing_input(&our_ipk, &to, reason, ts)).to_bytes()
    };
    let envelope = MlsEnvelopeP::PairDecline(PairDeclineP {
        sender_ipk: Bytes(our_ipk),
        recipient_ipk: Bytes(to),
        reason,
        timestamp: ts,
        sig: Bytes(sig),
    });
    let env_bytes = envelope.ser().map_err(|e| anyhow!("encode decline: {e}"))?;
    dispatch_envelope(to, our_ipk, &ipk_signer, env_bytes, false, Some(OpType::Control)).await
}

/// Emit an ephemeral activity signal to `peer` — an OR of `ACTIVITY_*` bits
/// (`0` = present-idle). Fire-and-forget over the relay, cleartext (not MLS);
/// dropped if we're offline or the peer isn't online. The relay never queues it.
pub async fn set_activity(conversation: [u8; 16], activity: u16) -> Result<()> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ts = crate::utils::systime().as_millis() as u64;

    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else { return Ok(()) };

    // Ephemeral and per-recipient: the signature binds `to`, so every member
    // gets their own. Unlike content there is nothing to encrypt and nothing to
    // outbox — a typing signal that misses its moment is worthless.
    for peer in Conversation::recipients(&conversation) {
        let sig = crate::data::identity::IdentitySigner::sign(&activity_sig_message(
            &peer, &our_ipk, activity, ts,
        ))
        .map_err(|e| anyhow!("sign ephemeral: {e}"))?
        .to_bytes();
        let eph = ActivityP {
            to: Bytes(peer),
            from: Bytes(our_ipk),
            activity,
            timestamp: ts,
            sig: Bytes(sig),
        };
        let bytes =
            CRelayPacket::Activity(eph).pack().map_err(|e| anyhow!("pack ephemeral: {e}"))?;
        if let Ok((mut tx, _rx)) = conn.open_bi().await {
            let _ = tx.write_all(&bytes).await;
            let _ = tx.finish();
        }
    }
    Ok(())
}

/// Subscribe to presence for `contacts` (replaces the prior interest set).
/// Fire-and-forget: the relay pushes a snapshot then deltas via `on_presence`.
/// The UI re-calls this on connect and whenever the contact list changes.
pub async fn subscribe_presence(contacts: Vec<[u8; 32]>) -> Result<()> {
    let previous = previous_presence_contacts()?;
    replace_presence_contacts(&contacts)?;
    send_presence_subscription(contacts, previous).await
}

/// Reissue a fresh lease while relay connection remains active. Contact state
/// is durable, so restarts retain revocation diff source-of-truth.
pub async fn renew_presence_lease() -> Result<()> {
    let contacts = persisted_presence_contacts()?;
    if contacts.is_empty() {
        return Ok(());
    }
    let previous = contacts.iter().copied().collect();
    send_presence_subscription(contacts, previous).await
}

async fn send_presence_subscription(
    contacts: Vec<[u8; 32]>, previous: std::collections::HashSet<[u8; 32]>,
) -> Result<()> {
    use common::proto::dht_p2p::PresenceConsent;
    use common::proto::dht_p2p::PresenceLease;
    use common::proto::dht_p2p::presence_consent_signing_input;
    use common::proto::dht_p2p::presence_lease_signing_input;
    use ed25519_dalek::Signer;
    let identity = Identity::get().ok_or_else(|| anyhow!("identity not found"))?;
    let me = identity.ipk();
    let signer = crate::data::identity::secret_key_signing(&me)?;
    let now = crate::utils::systime().as_millis() as u64;
    let relay_id = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.home_node_id)
    }
    .ok_or_else(|| anyhow!("presence requires DHT-enabled relay"))?;
    let relay_id = common::quic::id::NodeId::from_bytes(relay_id);
    let desired: std::collections::HashSet<_> = contacts.iter().copied().collect();
    let consents = desired
        .iter()
        .map(|recipient| (*recipient, true))
        .chain(previous.difference(&desired).map(|recipient| (*recipient, false)))
        .map(|(recipient, granted)| PresenceConsent {
            owner: me.into(),
            recipient: recipient.into(),
            version: now,
            issued_at_ms: now,
            granted,
            user_sig: signer
                .sign(&presence_consent_signing_input(&me, &recipient, now, now, granted))
                .to_bytes()
                .into(),
        })
        .collect();
    let version = next_presence_lease_version(now)?;
    let expires_at_ms = now + common::proto::dht_p2p::PRESENCE_LEASE_MAX_MS;
    let lease = PresenceLease {
        user: me.into(),
        relay_id,
        version,
        issued_at_ms: now,
        expires_at_ms,
        user_sig: signer
            .sign(&presence_lease_signing_input(&me, &relay_id, version, now, expires_at_ms))
            .to_bytes()
            .into(),
    };
    let sub =
        SubscribePresenceP { contacts: contacts.into_iter().map(Bytes).collect(), consents, lease };
    let bytes =
        CRelayPacket::SubscribePresence(sub).pack().map_err(|e| anyhow!("pack subscribe: {e}"))?;
    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else { return Ok(()) };
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = tx.write_all(&bytes).await;
        let _ = tx.finish();
    }
    Ok(())
}

fn persisted_presence_contacts() -> Result<Vec<[u8; 32]>> {
    let conn = crate::db::network::NETWORK_DB.lock();
    let mut stmt = conn.prepare("SELECT peer FROM presence_contacts")?;
    Ok(stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .filter_map(|peer| peer.ok().and_then(|peer| peer.try_into().ok()))
        .collect())
}

fn previous_presence_contacts() -> Result<std::collections::HashSet<[u8; 32]>> {
    Ok(persisted_presence_contacts()?.into_iter().collect())
}

fn replace_presence_contacts(contacts: &[[u8; 32]]) -> Result<()> {
    let mut conn = crate::db::network::NETWORK_DB.lock();
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM presence_contacts", [])?;
    for peer in contacts {
        tx.execute("INSERT OR IGNORE INTO presence_contacts(peer) VALUES (?1)", [peer.as_slice()])?;
    }
    tx.commit()?;
    Ok(())
}

fn next_presence_lease_version(now: u64) -> Result<u64> {
    let conn = crate::db::network::NETWORK_DB.lock();
    let old = conn
        .query_row("SELECT lease_version FROM presence_state WHERE singleton = 1", [], |row| row.get(0))
        .unwrap_or(0);
    let version = old.max(now).saturating_add(1);
    conn.execute(
        "INSERT INTO presence_state(singleton, lease_version) VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET lease_version = excluded.lease_version",
        [version],
    )?;
    Ok(version)
}

/// Process-wide fg/bg intent. Defaults to Idle: a bare/backgrounded process
/// (e.g. a headless push wake-drain) is Idle until the UI foregrounds it.
static PRESENCE_IDLE: AtomicBool = AtomicBool::new(true);

/// Whether the user is in the app. The renewal loop asks before spending a
/// lease renewal and a K-home publish on a connection nobody is looking at.
pub fn presence_is_active() -> bool {
    !PRESENCE_IDLE.load(Ordering::Relaxed)
}

/// Tell the relay our activity mode: `idle = true` on backgrounding (sent as
/// the last packet before the app freezes), `false` on return. Fire-and-forget;
/// the relay updates the state it reports to our mutual contacts.
pub async fn set_presence(idle: bool) -> Result<()> {
    PRESENCE_IDLE.store(idle, Ordering::Relaxed);
    send_presence(idle).await
}

/// Extend the claim: a fresh lease, then the state republished under it.
///
/// Order matters — `forward_to_homes` signs the state against whatever lease the
/// relay currently holds, and a home rejects a record whose lease has expired.
pub async fn renew_presence() -> Result<()> {
    renew_presence_lease().await?;
    send_presence(false).await
}

async fn send_presence(idle: bool) -> Result<()> {
    let mode = if idle {
        common::proto::client_rel::PresenceMode::Idle
    } else {
        common::proto::client_rel::PresenceMode::Active
    };
    let bytes =
        CRelayPacket::SetPresence(mode).pack().map_err(|e| anyhow!("pack set_presence: {e}"))?;
    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else { return Ok(()) };
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = tx.write_all(&bytes).await;
        let _ = tx.finish();
    }
    Ok(())
}

/// Re-assert presence on a relay (re)connect, but only when the user is
/// actually in the app. A fresh connection carries no `active_clients` entry, so
/// Idle is already the relay's default and announcing it would cost a K-home
/// publish to say nothing — presence is the user's intent, not the socket's.
pub async fn reassert_presence() -> Result<()> {
    if PRESENCE_IDLE.load(Ordering::Relaxed) {
        return Ok(());
    }
    set_presence(false).await
}

/// Pairing: fetch `to`'s KeyPackage, build the 1:1 group, and
/// publish a Welcome carrying `pairing` (our invite + name). The contact is
/// saved as **PENDING only on success** — an unreachable peer (KP not
/// published; the common new-user case) errors WITHOUT leaving a bricked
/// contact behind. `peer_name` is the invitee's self-asserted name for the
/// saved row. Entry point for `api::identity::pair_from_qr`.
pub async fn pair(to: [u8; 32], peer_name: String, pairing: PairingP) -> Result<()> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    let dht_client = {
        let guard = RELAY.read();
        guard.as_ref().and_then(|r| r.dht_client.clone())
    };
    let client =
        dht_client.ok_or_else(|| anyhow!("not connected to a relay; reconnect before pairing"))?;
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(stash_db_handle());
    let buffer = EpochCatchupBuffer::new(stash_db_handle());
    let ctx = MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };

    // `?` on a KP-fetch miss returns before any save — no bricked contact.
    let group = lazy_create_group_paired(&ctx, &our_ipk, &ipk_signer, &to, Some(pairing)).await?;
    Contact::save_pending(to, peer_name)?;
    if let Err(e) = Contact::set_mls_group_id(&to, &group.group_id()) {
        warn!("PAIR: persist mls_group_id failed: {e}");
    }
    Ok(())
}

/// Bundle of references threaded through the MLS-aware send/receive
/// paths. Held by reference so the caller controls lifetime; the
/// `'a` allows tests to plumb a stack-local fake without `Arc`-cloning.
pub struct MlsContext<'a, C: DhtClient> {
    pub provider: &'a PromtuzMlsProvider,
    pub stash:    &'a KeyPackageStash,
    pub buffer:   &'a EpochCatchupBuffer,
    pub dht:      &'a C,
}

/// Lazy-create the implicit 1:1 group with `to`. Returns the
/// freshly-built `MlsGroupHandle` plus the group_id we persisted to
/// the contact row.
///
/// Steps:
/// 1. Fetch the recipient's KeyPackage via the dialer.
/// 2. Mint a fresh group_id (`mint_group_id`).
/// 3. Build credential + leaf signing key.
/// 4. `MlsGroupHandle::create` (founder = us).
/// 5. `add_members` with the fetched KP → `(commit, welcome)`.
/// 6. Wrap welcome in `WelcomeEnvelopeP`; publish via `DhtClient`.
/// 7. Merge our pending commit; persist the group_id on the contact.
///
/// Exposed as `pub` so the e2e integration harness can drive the
/// production lazy-create flow without going through `send_message_inner`
/// (which depends on global `Identity::get()` / `RELAY` / `Contact` state).
/// Production callers continue to invoke it via `send_message_inner`; the
/// visibility has no behavioural impact.
pub async fn lazy_create_group<C: DhtClient>(
    ctx: &MlsContext<'_, C>, our_ipk: &[u8; 32], ipk_signer: &SigningKey, to: &[u8; 32],
) -> Result<MlsGroupHandle> {
    lazy_create_group_paired(ctx, our_ipk, ipk_signer, to, None).await
}

/// Like [`lazy_create_group`] but attaches `pairing` (an invite + sender
/// name) to the published Welcome, so a not-yet-contact recipient can
/// gate-accept it. The pairing flow (`api::identity::pair_from_qr`) uses
/// this; ordinary first-sends go through the no-pairing wrapper above.
/// Fetch and verify `who`'s published KeyPackage, returning it with the
/// `kp_ref` that names the exact record consumed (the stash needs it to mark
/// the key spent).
///
/// The `owner_sig` re-check is defence in depth: the home should have
/// validated it already, but a malicious replica could forward a
/// stale-but-tampered record.
pub(crate) async fn fetch_verified_keypackage<C: DhtClient>(
    ctx: &MlsContext<'_, C>, who: &[u8; 32],
) -> Result<(KeyPackage, [u8; 32])> {
    // Keep the concrete `DhtClientError` downcastable through the anyhow chain
    // (do NOT stringify): `send_payload` inspects it to detect a `NoStash`
    // KP-miss and defer the send instead of hard-failing.
    let fetched = ctx
        .dht
        .fetch_keypackage_for(who)
        .await
        .map_err(|e| anyhow::Error::new(e).context("fetch_keypackage_for"))?;
    {
        use common::proto::mls_wire::MLS_WIRE_VERSION;
        use common::proto::mls_wire::kp_record_signing_input;
        let vk = VerifyingKey::from_bytes(&fetched.record.ipk.0)
            .map_err(|e| anyhow!("recipient ipk is not valid Ed25519: {e}"))?;
        if &fetched.record.ipk.0 != who {
            bail!("fetched KP's owner ipk does not match the member requested");
        }
        let sig = Signature::from_bytes(&fetched.record.owner_sig.0);
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &fetched.record.ipk.0,
            &fetched.record.kp_ref.0,
            &fetched.record.kp_bytes.0,
            fetched.record.expires_at_ms,
        );
        // `verify_strict` rejects non-canonical sigs and small-order R values;
        // mirrors the relay-side discipline.
        vk.verify_strict(&msg, &sig).map_err(|e| anyhow!("owner_sig invalid: {e}"))?;
    }
    let kp = decode_keypackage_bytes(&fetched.record.kp_bytes.0)
        .map_err(|e| anyhow!("decode KP: {e}"))?;
    let kp_ref: [u8; 32] = {
        let r = kp.hash_ref(ctx.provider.crypto()).map_err(|e| anyhow!("kp hash_ref: {e:?}"))?;
        let mut out = [0u8; 32];
        let s = r.as_slice();
        let copy = s.len().min(32);
        out[..copy].copy_from_slice(&s[..copy]);
        out
    };
    Ok((kp, kp_ref))
}

pub async fn lazy_create_group_paired<C: DhtClient>(
    ctx: &MlsContext<'_, C>, our_ipk: &[u8; 32], ipk_signer: &SigningKey, to: &[u8; 32],
    pairing: Option<PairingP>,
) -> Result<MlsGroupHandle> {
    // 1. Fetch peer's KP.
    // Keep the concrete `DhtClientError` downcastable through the anyhow
    // chain (do NOT stringify): `attempt_send` inspects it to detect a
    // `NoStash` KP-miss and defer the send instead of hard-failing.
    let fetched = ctx
        .dht
        .fetch_keypackage_for(to)
        .await
        .map_err(|e| anyhow::Error::new(e).context("fetch_keypackage_for"))?;

    // Verify the per-record `owner_sig` re-validates under the peer's
    // IPK (defence-in-depth — the home should have already done this,
    // but a malicious replica might forward stale-but-tampered records).
    {
        use common::proto::mls_wire::MLS_WIRE_VERSION;
        use common::proto::mls_wire::kp_record_signing_input;
        let vk = VerifyingKey::from_bytes(&fetched.record.ipk.0)
            .map_err(|e| anyhow!("recipient ipk is not valid Ed25519: {e}"))?;
        if &fetched.record.ipk.0 != to {
            bail!("fetched KP's owner ipk does not match `to`");
        }
        let sig = Signature::from_bytes(&fetched.record.owner_sig.0);
        // The transcript folds `BLAKE3(kp_bytes)` so the full record
        // (incl. body) is bound by `owner_sig`.
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &fetched.record.ipk.0,
            &fetched.record.kp_ref.0,
            &fetched.record.kp_bytes.0,
            fetched.record.expires_at_ms,
        );
        // `verify_strict` rejects non-canonical sigs and small-order R
        // values; mirrors the relay-side discipline.
        vk.verify_strict(&msg, &sig).map_err(|e| anyhow!("owner_sig invalid: {e}"))?;
    }

    let kp = decode_keypackage_bytes(&fetched.record.kp_bytes.0)
        .map_err(|e| anyhow!("decode KP: {e}"))?;
    let kp_ref_used: [u8; 32] = {
        let r = kp.hash_ref(ctx.provider.crypto()).map_err(|e| anyhow!("kp hash_ref: {e:?}"))?;
        let mut out = [0u8; 32];
        let s = r.as_slice();
        let copy = s.len().min(32);
        out[..copy].copy_from_slice(&s[..copy]);
        out
    };

    // 2. Mint group id.
    let group_id = mint_group_id(our_ipk);

    // 3. Build credential + leaf signer.
    let (leaf_kp, _cwk_unused) =
        build_self_credential(our_ipk).map_err(|e| anyhow!("build credential: {e}"))?;
    leaf_kp.store(ctx.provider.storage()).map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

    // 4. Create group with us as founder.
    let mut group =
        MlsGroupHandle::create(ctx.provider, &leaf_kp, our_ipk, leaf_kp.public(), &group_id)
            .map_err(|e| anyhow!("create group: {e}"))?;

    // 5. Add the recipient via their KP.
    let (_commit, welcome) = group
        .add_members(ctx.provider, &leaf_kp, &[kp])
        .map_err(|e| anyhow!("add_members: {e}"))?;

    // 6. Wrap + publish welcome.
    let mut env = make_welcome_envelope(welcome, group_id, *our_ipk, *to, kp_ref_used, ipk_signer)
        .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
    env.pairing = pairing;

    if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
        // Roll back the half-built group state before surfacing — the
        // openmls storage already holds tree, secrets, leaf node etc.
        // for `group_id`; leaving them behind would let a retry
        // attempt to re-create with the same id and trip openmls.
        if let Err(de) = group.delete(ctx.provider) {
            warn!("MLS: oversize-welcome rollback failed: {de}");
        }
        bail!(
            "welcome blob {} exceeds MAX_WELCOME_BYTES = {}",
            env.welcome_blob.0.len(),
            MAX_WELCOME_BYTES
        );
    }

    // Publish failure must roll back the founder's group state.
    // Otherwise `send_message_inner` would persist `mls_group_id` on
    // the contact row pointing at a group the
    // recipient never received the Welcome for; subsequent sends would
    // succeed encrypting against a one-member group and the recipient
    // would never decrypt anything. The K-fan-out RPC now returns
    // `Err(QuorumNotMet { … })` rather than `Ok(Failed)`, so any
    // error here is load-bearing.
    // Live-push the Welcome (same Dispatch path as messages). On failure roll
    // back the group: else we persist an mls_group_id the peer never got a
    // Welcome for, and every later send encrypts to a group of one.
    if let Err(e) = ctx.dht.deliver_welcome(&env).await {
        if let Err(de) = group.delete(ctx.provider) {
            warn!("MLS: welcome-delivery rollback of group state failed: {de}");
        }
        return Err(anyhow!("deliver_welcome: {e}"));
    }

    // 7. Merge our own commit.
    group.merge_pending_commit(ctx.provider).map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

    // Caller (send_message_inner) persists the group_id on the contact
    // row. Keeping the Contact-DB write out of this helper keeps it
    // unit-testable without the global CONTACTS_DB lazy static.

    Ok(group)
}

/// Encrypt `plaintext` for `group`, then wrap into a
/// [`MlsApplicationEnvelopeP`] signed under the sender's IPK (binds `to_ipk`).
///
/// Returns the postcard-encoded envelope bytes ready to drop into
/// `DispatchP::payload`. Caller signs the outer DispatchP separately.
///
/// Exposed as `pub` so the e2e harness can build outgoing envelopes
/// without going through `send_message_inner` (which fetches IPK via
/// the global `Identity` table).
/// One MLS ciphertext, not yet addressed to anybody.
///
/// The group encrypts a logical message exactly once; the transport then
/// unicasts that same ciphertext to each member, re-signing per recipient
/// because `envelope_signing_input` binds a single `to_ipk`. Encrypting per
/// recipient instead would advance the sender's ratchet N times per message
/// and leave every member with N-1 generations they can never account for.
pub struct SealedMessage {
    group_id:  [u8; 32],
    epoch:     u64,
    mls_bytes: Vec<u8>,
}

/// Encrypt `plaintext` to the group once, advancing the ratchet one step.
pub fn seal_application_message<C: DhtClient>(
    ctx: &MlsContext<'_, C>, group: &mut MlsGroupHandle,
    leaf_signer: &openmls_basic_credential::SignatureKeyPair, plaintext: &[u8],
) -> Result<SealedMessage, MlsGroupError> {
    let mls_msg = group.create_application_message(ctx.provider, leaf_signer, plaintext)?;
    let mls_bytes = mls_msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)?;

    if mls_bytes.len() > MAX_FRAMED_MLS_BYTES {
        return Err(MlsGroupError::Internal(format!(
            "mls_bytes {} exceeds MAX_FRAMED_MLS_BYTES = {}",
            mls_bytes.len(),
            MAX_FRAMED_MLS_BYTES
        )));
    }

    Ok(SealedMessage { group_id: group.group_id(), epoch: group.epoch(), mls_bytes })
}

impl SealedMessage {
    /// Wrap an already-produced MLS message — a Commit from a membership
    /// change — for the same per-recipient fan-out an application message
    /// gets. Commits ride the Application envelope: its inner discriminator
    /// is openmls's business, and the receiving side already dispatches a
    /// `StagedCommitMessage` to `merge_staged_commit`. No new wire type.
    ///
    /// `epoch` must be the epoch the Commit was *created* in, i.e. before the
    /// sender merges it — that is the epoch recipients still hold when it
    /// arrives.
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
        Ok(SealedMessage { group_id, epoch, mls_bytes })
    }

    /// Wrap the sealed ciphertext in an envelope addressed to one member.
    /// Cheap — a signature over a transcript, no MLS state touched — so
    /// calling it once per member is the whole cost of fan-out.
    pub fn address_to(
        &self, to: &[u8; 32], ipk_signer: &SigningKey,
    ) -> Result<Vec<u8>, MlsGroupError> {
        use ed25519_dalek::Signer;
        let transcript = envelope_signing_input(
            PROTOCOL_VERSION,
            to,
            &self.group_id,
            self.epoch,
            &self.mls_bytes,
        );
        let outer_sig = ipk_signer.sign(&transcript);

        let env = MlsApplicationEnvelopeP {
            version:     MLS_ENVELOPE_VERSION,
            group_id:    self.group_id.into(),
            epoch:       self.epoch,
            mls_message: ByteVec(self.mls_bytes.clone()),
            sender_sig:  outer_sig.to_bytes().into(),
        };
        MlsEnvelopeP::Application(env)
            .ser()
            .map_err(|e| MlsGroupError::Internal(format!("postcard ser envelope: {e}")))
    }
}

/// Seal and address in one step — the 1:1 shape, and the shape tests use.
pub fn build_application_envelope_bytes<C: DhtClient>(
    ctx: &MlsContext<'_, C>, group: &mut MlsGroupHandle,
    leaf_signer: &openmls_basic_credential::SignatureKeyPair, our_ipk: &[u8; 32], to: &[u8; 32],
    plaintext: &[u8], ipk_signer: &SigningKey,
) -> Result<Vec<u8>, MlsGroupError> {
    let _ = our_ipk; // bound only via transcript, not the postcard wire
    seal_application_message(ctx, group, leaf_signer, plaintext)?.address_to(to, ipk_signer)
}

/// The MLS send path — the replacement for the v2-era shared-key
/// encryption.
///
/// Generic over the [`DhtClient`] backend so unit tests inject a
/// fake; production wires the real dialer.
pub async fn send_message_inner<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: [u8; 16], content: String, reply_to: Option<[u8; 16]>,
) -> Result<()> {
    // 0. Save to local DB first (status = pending), then drive one attempt.
    let msg = Message::save_outgoing(conversation, &content, reply_to)?;
    attempt_send(ctx, conversation, msg).await
}

/// Sync, pure-DB prep for an outgoing image: persist the optimistic caption
/// row plus a PLACEHOLDER `message_media` side row (null blob — the encode
/// hasn't run yet), so the bubble shows the instant the user hits send.
/// [`finish_image`] lands the bytes and drives the send.
pub(crate) fn build_image_message(
    conversation: [u8; 16], width: u32, height: u32, caption: &str, group_id: Option<[u8; 16]>,
) -> Result<Message> {
    crate::data::media::save_outgoing_with_media(&conversation, caption, None, &crate::data::media::MediaRow {
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
    })
}

/// Sync, pure-DB prep for an outgoing attachment: persist the optimistic
/// caption row plus a PLACEHOLDER `message_media` side row (thumb, name, size,
/// mime — null file_id; the manifest hash hasn't run yet). Mirrors
/// [`build_image_message`]; [`finish_attachment`] lands the file_id and sends.
pub(crate) fn build_attachment_message(
    conversation: [u8; 16], size: u64, name: &str, mime: &str, thumb: Option<Vec<u8>>,
    caption: &str, group_id: Option<[u8; 16]>,
) -> Result<Message> {
    crate::data::media::save_outgoing_with_media(&conversation, caption, None, &crate::data::media::MediaRow {
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
    })
}

/// Finalize an optimistic image placeholder: land the compressed bytes on the
/// row, then send. Called after the sync compress succeeds.
pub(crate) async fn finish_image(
    conversation: [u8; 16], did: [u8; 16], avif: Vec<u8>, width: u32, height: u32,
) -> Result<()> {
    crate::data::media::set_blob(&conversation, &did, &avif, width, height)?;
    let msg = Message::get_by_dispatch(&conversation, &did)
        .ok_or_else(|| anyhow!("image row vanished"))?;
    // ponytail: payload rebuilt from the just-finalized row via
    // rebuild_pending_payload instead of threading caption/group_id through —
    // one small read, and finish + retry share one builder by construction.
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_prepared(conversation, &msg, payload_bytes).await
}

/// Finalize an optimistic attachment placeholder: land the content-addressed
/// file_id on the row, then send. Called after the manifest pass succeeds.
pub(crate) async fn finish_attachment(
    conversation: [u8; 16], did: [u8; 16], file_id: [u8; 32],
) -> Result<()> {
    crate::data::media::set_file_id(&conversation, &did, &file_id)?;
    let msg = Message::get_by_dispatch(&conversation, &did)
        .ok_or_else(|| anyhow!("attachment row vanished"))?;
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_prepared(conversation, &msg, payload_bytes).await
}

/// Drive one send attempt for an already-persisted outgoing `msg` row:
/// contact lookup → group resolve/lazy-create → envelope → outbox enqueue
/// → wire send → ack. Split out of [`send_message_inner`] so
/// [`retry_pending_sends`] can re-run a still-`pending` row on reconnect
/// without re-inserting it. Rebuilds the payload from the row (via
/// [`rebuild_pending_payload`], which re-emits an `Image` for a row that has a
/// stored media side, else `Text`/`Reply`) so a retry reuses the same dispatch
/// id and the recipient dedups it, then hands off to [`send_payload`] for the
/// group-resolve/envelope/wire tail shared with [`send_prepared`].
pub async fn attempt_send<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: [u8; 16], msg: Message,
) -> Result<()> {
    let payload_bytes = rebuild_pending_payload(&conversation, &msg)?;
    send_payload(ctx, conversation, &msg, payload_bytes).await
}

/// Persist an inbound [`Body`] under `did`, handing back the stored row with the
/// text to surface (a caption, for media). Single home for the storage shape:
/// pre-v12 payloads convert through [`legacy_body`] and land here too, so the
/// wire vintage stops being visible past this point.
pub(crate) fn save_inbound_body(
    conversation: &[u8; 16], sender: &[u8; 32], did: &[u8; 16], timestamp: u64,
    reply_to: Option<[u8; 16]>, body: Body,
) -> Result<Option<(Message, String)>> {
    let Some((content, media)) = split_body(body) else { return Ok(None) };
    Ok(match media {
        None => Message::save_incoming(*conversation, *sender, did, &content, timestamp, reply_to)?
            .map(|m| (m, content)),
        Some(r) => crate::data::media::save_incoming_with_media(
            conversation,
            sender,
            did,
            &content,
            timestamp,
            reply_to,
            &r,
        )?
        .map(|m| (m, content)),
    })
}

/// A body as storage holds it: the text to surface (a caption, for media) and
/// the media side-row it needs, if any. `None` for a sticker — pack
/// distribution isn't built, so there's nothing to resolve an id against and a
/// stored row would be unrenderable.
fn split_body(body: Body) -> Option<(String, Option<crate::data::media::MediaRow>)> {
    use crate::data::media::KIND_ATTACHMENT;
    use crate::data::media::KIND_IMAGE;
    use crate::data::media::MediaRow;

    Some(match body {
        Body::Text(content) => (content, None),
        Body::Image { caption, group_id, mime, width, height, data } => (
            caption,
            Some(MediaRow {
                kind: KIND_IMAGE,
                group_id: group_id.map(|g| g.to_vec()),
                mime,
                name: String::new(),
                size: data.len() as u64,
                width,
                height,
                blob: Some(data),
                thumb: None,
                file_id: None,
            }),
        ),
        Body::Attachment { caption, group_id, mime, name, size, thumb, file_id } => (
            caption,
            Some(MediaRow {
                kind: KIND_ATTACHMENT,
                group_id: group_id.map(|g| g.to_vec()),
                mime,
                name,
                size,
                width: 0,
                height: 0,
                blob: None,
                thumb: (!thumb.is_empty()).then_some(thumb),
                file_id: Some(file_id.to_vec()),
            }),
        ),
        Body::Sticker { .. } => return None,
    })
}

/// Apply a [`AppPayload::Revise`] over its target: refuse the swaps the matrix
/// disallows, then persist the new body in place. `own = false` on the receive
/// path — a peer may only revise messages IT sent us. `None` when the target is
/// unknown, tombstoned, or authored by the other party.
pub(crate) fn apply_revise_body(
    conversation: &[u8; 16], target: &[u8; 16], body: Body, own: bool,
) -> Result<Option<(crate::db::messages::MessageRow, String)>> {
    let current = BodyKind::stored(crate::data::media::get(conversation, target)?.map(|m| m.kind));
    let incoming = BodyKind::of(&body);
    if !current.revisable_to(incoming) {
        bail!("revision {current:?} -> {incoming:?} is not permitted");
    }
    let Some((content, media)) = split_body(body) else { bail!("sticker revision unsupported") };
    Ok(crate::data::media::apply_revise(conversation, target, &content, media.as_ref(), own)?
        .map(|row| (row, content)))
}

/// Pre-v12 content payload → the [`Body`] + quote target it was expressing.
/// `None` for anything that isn't content (receipts, control, P2P).
pub(crate) fn legacy_body(p: AppPayload) -> Option<(Option<[u8; 16]>, Body)> {
    Some(match p {
        AppPayload::Text(content) => (None, Body::Text(content)),
        AppPayload::Reply { reply_to, content } => (Some(reply_to), Body::Text(content)),
        AppPayload::Image { caption, group_id, mime, width, height, data } => {
            (None, Body::Image { caption, group_id, mime, width, height, data })
        },
        AppPayload::Attachment { caption, group_id, mime, name, size, thumb, file_id } => {
            (None, Body::Attachment { caption, group_id, mime, name, size, thumb, file_id })
        },
        _ => return None,
    })
}

/// A body's revision class. Storage keeps a media side-row kind rather than a
/// [`Body`], so the compatibility rule is expressed over this discriminant and
/// both sides can name it without materialising bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyKind {
    Text,
    Image,
    Attachment,
    Sticker,
}

impl BodyKind {
    pub(crate) fn of(body: &Body) -> Self {
        match body {
            Body::Text(..) => Self::Text,
            Body::Image { .. } => Self::Image,
            Body::Attachment { .. } => Self::Attachment,
            Body::Sticker { .. } => Self::Sticker,
        }
    }

    /// What a stored message already is: its media side-row kind, or text when
    /// it has none.
    pub(crate) fn stored(media_kind: Option<u8>) -> Self {
        match media_kind {
            Some(crate::data::media::KIND_IMAGE) => Self::Image,
            Some(crate::data::media::KIND_ATTACHMENT) => Self::Attachment,
            _ => Self::Text,
        }
    }

    /// Which swaps an [`AppPayload::Revise`] may make. The line that matters is
    /// how a body reaches the peer: text and image both ride inside the frame
    /// they already hold, so those interchange freely. An attachment is fetched
    /// device-to-device by `file_id` — revising into one hands the peer a
    /// transfer for a message they consider delivered, and revising out of one
    /// orphans a transfer they may be mid-download on. Stickers are atomic (no
    /// caption, nothing to pair with text), so they only revise to a sticker.
    pub(crate) fn revisable_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Text | Self::Image, Self::Text | Self::Image)
                | (Self::Attachment, Self::Attachment)
                | (Self::Sticker, Self::Sticker)
        )
    }
}

/// Reconstruct the wire [`AppPayload::Post`] for a pending outgoing row on
/// (re)send. A row carrying a stored `KIND_IMAGE` media side-row resends as
/// [`Body::Image`] (caption + AVIF blob), so a first-send deferred while the
/// peer had no published KeyPackage doesn't silently downgrade to a
/// bare-caption text. Everything else — including media kinds not yet re-driven
/// here, whose bytes live off-row — falls through to [`Body::Text`]. The quote
/// target rides the envelope, so it survives on every body kind.
pub(crate) fn rebuild_pending_payload(
    conversation: &[u8; 16], msg: &Message,
) -> Result<Vec<u8>> {
    let did: Option<[u8; 16]> = msg.inner.dispatch_id.as_deref().and_then(|r| r.try_into().ok());
    let media = did.and_then(|d| crate::data::media::get(conversation, &d).ok().flatten());
    let reply_to: Option<[u8; 16]> =
        msg.inner.reply_to.as_deref().and_then(|r| r.try_into().ok());
    let body = match media {
        Some(m) if m.kind == crate::data::media::KIND_IMAGE => {
            // Empty blob = un-finalized placeholder (compress still running).
            // Bail so a reconnect retry leaves the row pending; the finish_*
            // path drives the real send.
            let data = match m.blob {
                Some(b) if !b.is_empty() => b,
                _ => bail!("media not ready"),
            };
            Body::Image {
                caption:  msg.inner.content.clone(),
                group_id: m.group_id.as_deref().and_then(|g| g.try_into().ok()),
                mime:     m.mime,
                width:    m.width,
                height:   m.height,
                data,
            }
        },
        Some(m) if m.kind == crate::data::media::KIND_ATTACHMENT => {
            // Null file_id = un-finalized placeholder (manifest still hashing).
            let file_id = match m.file_id.as_deref().and_then(|f| f.try_into().ok()) {
                Some(f) => f,
                None => bail!("media not ready"),
            };
            Body::Attachment {
                caption:  msg.inner.content.clone(),
                group_id: m.group_id.as_deref().and_then(|g| g.try_into().ok()),
                mime:     m.mime,
                name:     m.name,
                size:     m.size,
                thumb:    m.thumb.unwrap_or_default(),
                file_id,
            }
        },
        _ => Body::Text(msg.inner.content.clone()),
    };
    AppPayload::Post { reply_to, body }.ser().map_err(|e| anyhow!("encode AppPayload: {e}"))
}

/// Shared tail of [`attempt_send`] and [`send_prepared`]: resolve or
/// lazy-create the 1:1 MLS group with `to`, encrypt `payload_bytes` (an
/// already-serialized `AppPayload`) into an application envelope, and drive
/// one send attempt (durable outbox enqueue → wire send → ack handling).
/// `msg` supplies the row identity (`id`/`content`/`dispatch_id`) for
/// mark_sent/mark_failed/`MessageEv` — the payload itself is opaque here.
/// Resolve the MLS group backing `conversation`, lazy-creating it on a direct
/// chat's first send.
///
/// Serialized on a per-conversation lock for the whole "check existing →
/// fetch KP → build group → publish Welcome → bind" critical section, so two
/// concurrent first-sends can't both lazy-create. The lock never crosses an
/// `await` outside that section.
///
/// A group conversation is never lazy-created: its MLS group is minted
/// explicitly at creation time, with Welcomes to every founding member.
async fn group_for_conversation<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: &[u8; 16], our_ipk: &[u8; 32], ipk_signer: &SigningKey,
) -> Result<MlsGroupHandle> {
    let lock = group_create_lock(conversation);
    let _guard = lock.lock().await;

    // Re-read under the lock — a racing send may have just bound a group.
    let row = Conversation::get(conversation)
        .ok_or_else(|| anyhow!("no such conversation {}", hex::encode(&conversation[..4])))?;
    let bound: Option<[u8; 32]> =
        row.mls_group_id.as_ref().and_then(|g| g.as_slice().try_into().ok());

    if let Some(gid) = bound {
        match MlsGroupHandle::load(ctx.provider, &gid) {
            Ok(Some(g)) => return Ok(g),
            // The conversation points at a group we no longer hold state for
            // — openmls storage and SQLite drifted. Re-establish and repoint.
            // History is keyed on the conversation, so it rides through
            // untouched; only the pointer moves.
            Ok(None) => warn!("MESSAGE: conversation's group has no local state; recreating"),
            Err(e) => bail!("load group: {e}"),
        }
    }

    if row.kind == crate::data::conversation::KIND_GROUP {
        bail!("group conversation has no MLS group; it must be created explicitly");
    }
    let peer = Conversation::peer_of(conversation)
        .ok_or_else(|| anyhow!("direct conversation has no peer to pair with"))?;

    let group = lazy_create_group(ctx, our_ipk, ipk_signer, &peer).await?;
    Conversation::bind_group(conversation, &group.group_id())?;
    // Keep the address book's shortcut in step so pairing-era lookups agree.
    if let Err(e) = Contact::set_mls_group_id(&peer, &group.group_id()) {
        warn!("MESSAGE: persist mls_group_id failed: {e}");
    }
    Ok(group)
}

/// Sign, frame, enqueue and send one member's copy of an already-sealed
/// dispatch. Returns the relay's durability verdict; `Silence` covers every
/// transport failure, which leaves the outbox row for the reconciler.
pub(crate) async fn dispatch_to_member(
    to: &[u8; 32], our_ipk: &[u8; 32], ipk_signer: &SigningKey, id: &[u8; 16], payload: Vec<u8>,
    op: OpType, wake: bool,
) -> LastOutcome {
    let sig_message = dispatch_sig_message(to, our_ipk, id, &payload);
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&sig_message).to_bytes()
    };
    let fwd = DispatchP {
        to:             Bytes(*to),
        from:           Bytes(*our_ipk),
        id:             Bytes(*id),
        payload:        ByteVec(payload),
        sig:            Bytes(sig),
        accepted_at_ms: 0,
        wake,
    };
    // Frame once, enqueue before the wire. `.pack()` (not `.ser()`) yields the
    // length-prefixed bytes `send()` writes; the relay's read side is
    // length-prefixed, so storing raw postcard would desync every frame. Store
    // framed, send framed, reconciler re-sends framed — all byte-identical.
    let Ok(bytes) = CRelayPacket::Dispatch(fwd).pack() else {
        return LastOutcome::Terminal;
    };
    delivery::enqueue(id, op, Some(*to), &bytes);

    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else {
        info!("MESSAGE: offline — {} queued in outbox", hex::encode(&to[..4]));
        return LastOutcome::Silence;
    };
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        debug!("MESSAGE: {} send stream failed to open; left in outbox", hex::encode(&to[..4]));
        return LastOutcome::Silence;
    };
    if send.write_all(&bytes).await.is_err() || send.finish().is_err() {
        debug!("MESSAGE: {} interrupted mid-send; left in outbox", hex::encode(&to[..4]));
        return LastOutcome::Silence;
    }
    match SRelayPacket::unpack(&mut recv).await {
        Ok(SRelayPacket::DispatchAck(ack)) => {
            let outcome = delivery::outcome_for_ack(&ack);
            if matches!(outcome, LastOutcome::Durable) {
                delivery::retire(id, Some(*to));
                LAST_ACCEPTED_AT.lock().insert(*id, delivery::accepted_at_secs(&ack).unwrap_or(0));
            } else if matches!(outcome, LastOutcome::Terminal) {
                delivery::retire(id, Some(*to));
            }
            outcome
        },
        _ => {
            debug!("MESSAGE: {} no usable relay ack; left in outbox", hex::encode(&to[..4]));
            LastOutcome::Silence
        },
    }
}

/// Relay acceptance timestamps observed during the current fan-out, keyed by
/// dispatch id. The message's `sent` timestamp is the first member's ack; the
/// rest of the fan-out is the same logical send at the same moment.
static LAST_ACCEPTED_AT: Lazy<PlMutex<HashMap<[u8; 16], u64>>> =
    Lazy::new(|| PlMutex::new(HashMap::new()));

/// Encrypt an already-persisted message once and unicast it to every member.
///
/// The message settles only when the whole fan-out has drained: one member's
/// relay accepting is not a sent message when two others are still queued.
/// Any member left unacked stays in the outbox for `delivery::reconcile`.
async fn send_payload<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: [u8; 16], msg: &Message, payload_bytes: Vec<u8>,
) -> Result<()> {
    let msg_id = msg.inner.id;
    let content = &msg.inner.content;

    let recipients = Conversation::recipients(&conversation);
    if recipients.is_empty() {
        Message::mark_failed(&msg_id);
        MessageEv::Failed { id: msg_id, conversation, reason: "conversation has no members".into() }
            .emit();
        return Err(anyhow!("conversation has no members"));
    }

    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    // Decrypt the IPK secret ONCE and reuse the signer for every signature in
    // this send (one application seal + one DispatchP per member). Each decrypt
    // is a StrongBox op (~1s); doing it per signature would make a group send
    // take seconds per member. Zeroized on drop.
    let ipk_signer: SigningKey = crate::data::identity::secret_key_signing(&our_ipk)?;

    let mut group = match group_for_conversation(ctx, &conversation, &our_ipk, &ipk_signer).await {
        Ok(g) => g,
        Err(e) => {
            // A missing peer KeyPackage is transient (the peer republishes on
            // its own reconnect). Leave the message PENDING and let
            // retry_pending_sends re-run on the next reconnect — failing here
            // permanently is the "can't pair" bug. Safe: the fetch fails
            // BEFORE any group state is built, so no duplicate group.
            if e.chain()
                .any(|c| matches!(c.downcast_ref::<DhtClientError>(), Some(DhtClientError::NoStash)))
            {
                info!("MESSAGE: recipient has no published KP yet — left pending, will retry");
                return Ok(());
            }
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, conversation, reason: e.to_string() }.emit();
            return Err(e);
        },
    };

    // The leaf signing key for our seat in this group.
    let leaf_kp = leaf_signer_for_group(ctx.provider, &group, &our_ipk)?;

    // Encrypt once. The caller (attempt_send / finish_image / finish_attachment)
    // rebuilds `payload_bytes` from the row on every retry, so a resend reuses
    // the same dispatch id below and recipients dedup it.
    let sealed =
        seal_application_message(ctx, &mut group, &leaf_kp, &payload_bytes).map_err(|e| {
            Message::mark_failed(&msg_id);
            anyhow!("seal message: {e}")
        })?;

    let id: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .expect("save_outgoing always mints a dispatch_id")
        .try_into()
        .expect("dispatch_id is 16 bytes");

    let mut terminal = false;
    for to in &recipients {
        let payload = sealed
            .address_to(to, &ipk_signer)
            .map_err(|e| anyhow!("address envelope to member: {e}"))?;
        // New content: push-wake an offline member.
        let outcome =
            dispatch_to_member(to, &our_ipk, &ipk_signer, &id, payload, OpType::Message, true).await;
        terminal |= matches!(outcome, LastOutcome::Terminal);
    }

    // Settle only once no member's copy is left queued.
    if delivery::any_pending(&id) {
        return Ok(());
    }
    let accepted = LAST_ACCEPTED_AT.lock().remove(&id);
    match accepted {
        Some(timestamp) => {
            Message::mark_sent(&msg_id, timestamp);
            info!("MESSAGE: {} sent to {} member(s)", hex::encode(&id[..4]), recipients.len());
            MessageEv::Sent { id: msg_id, conversation, content: content.clone(), timestamp }.emit();
        },
        None if terminal => {
            Message::mark_failed(&msg_id);
            warn!("MESSAGE: {} rejected by relay", hex::encode(&id[..4]));
            MessageEv::Failed { id: msg_id, conversation, reason: "relay rejected".into() }.emit();
        },
        None => {},
    }
    Ok(())
}

/// Re-drive every still-`pending` first-send whose contact has no MLS
/// group yet — the messages [`attempt_send`] deferred because the peer
/// had not published a KeyPackage. Run once per reconnect (after the
/// welcome poll, which may itself have just paired us and unstuck one).
///
/// A pending row that already HAS a group is in the durable outbox;
/// `delivery::reconcile` owns it, so we skip it here to avoid a double
/// send. Per-message errors are logged and swallowed — one peer still
/// missing its KP must not block retrying the rest.
///
/// Re-drives via [`attempt_send`], which rebuilds the row's original payload
/// (an `Image` row resends its stored picture, not a bare-caption `Text`).
pub async fn retry_pending_sends<C: DhtClient>(ctx: &MlsContext<'_, C>) {
    // Snapshot the no-group rows BEFORE attempting any. The first send in a
    // conversation binds its MLS group, so a live per-row check would skip
    // every *later* deferred message in that same conversation — orphaning it
    // (pending, but never enqueued to the outbox, so `reconcile` can't send it
    // either). `attempt_send` handles a now-existing group fine, so attempting
    // all of this snapshot is safe.
    let deferred: Vec<_> = Message::pending_outgoing()
        .into_iter()
        .filter(|row| Conversation::group_of(&row.conversation_id).is_none())
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

/// Pull the leaf signing keypair for *our* member seat in `group`
/// out of openmls's storage. Used after `MlsGroupHandle::load` to
/// rebuild a usable `Signer` for outgoing application messages.
///
/// Exposed as `pub` so the e2e harness can re-derive the leaf signer
/// for a loaded group without touching `Identity::get()`.
pub fn leaf_signer_for_group(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, our_ipk: &[u8; 32],
) -> Result<openmls_basic_credential::SignatureKeyPair> {
    let leaf_idx = group
        .member_index_by_ipk(our_ipk)
        .ok_or_else(|| anyhow!("our IPK is not a member of group"))?;
    // Find our own credential's signature key.
    let pub_key: Vec<u8> = group
        .members()
        .find(|m| m.index == leaf_idx)
        .map(|m| m.signature_key)
        .ok_or_else(|| anyhow!("could not enumerate our member"))?;
    openmls_basic_credential::SignatureKeyPair::read(
        provider.storage(),
        &pub_key,
        SignatureScheme::ED25519,
    )
    .ok_or_else(|| anyhow!("leaf signing key not in storage"))
}

/// Decode and dispatch an inbound MLS envelope from a `DispatchP::payload`.
///
/// Called by `quic/server.rs::handle_deliver` after it has confirmed
/// the outer `DispatchP::sig` is valid (the existing v2 path also did
/// this — we keep that contract).
///
/// **Generic over the dialer** so the test surface can drive
/// process_inbound_envelope with a `FakeDhtClient` for the `on_consumed`
/// callback path.
pub async fn process_inbound_envelope<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], payload: &[u8], accepted_at_ms: u64,
) -> Result<Option<InboundDecoded>> {
    let envelope =
        MlsEnvelopeP::deser(payload).map_err(|e| anyhow!("postcard deser MlsEnvelopeP: {e}"))?;

    // Enforce inner-byte size caps at the decode boundary. Bigger
    // blobs are dropped before openmls touches them — defends against
    // a malicious peer crafting an oversize Welcome/Application to
    // amplify recipient CPU on TLS deserialise.
    match &envelope {
        MlsEnvelopeP::Welcome(env) => {
            if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
                bail!(
                    "inbound welcome_blob {} exceeds MAX_WELCOME_BYTES = {}",
                    env.welcome_blob.0.len(),
                    MAX_WELCOME_BYTES
                );
            }
        },
        MlsEnvelopeP::Application(env) => {
            if env.mls_message.0.len() > MAX_FRAMED_MLS_BYTES {
                bail!(
                    "inbound mls_message {} exceeds MAX_FRAMED_MLS_BYTES = {}",
                    env.mls_message.0.len(),
                    MAX_FRAMED_MLS_BYTES
                );
            }
        },
        MlsEnvelopeP::PairDecline(_) => {}, // fixed-size, no cap
    }

    match envelope {
        MlsEnvelopeP::Welcome(env) => match process_welcome_inbound(ctx, sender_ipk, env)? {
            WelcomeOutcome::Accepted => Ok(Some(InboundDecoded::Welcome)),
            WelcomeOutcome::Rejected(reason) => {
                Ok(Some(InboundDecoded::WelcomeRejected { sender_ipk, reason }))
            },
        },
        MlsEnvelopeP::Application(env) => {
            let decoded = process_application_inbound(ctx, sender_ipk, env, accepted_at_ms)?;
            if let InboundDecoded::ApplicationNoGroup { group_id } = &decoded {
                heal_dead_group(ctx, sender_ipk, group_id).await;
            }
            Ok(Some(decoded))
        },
        MlsEnvelopeP::PairDecline(d) => {
            process_pair_decline_inbound(sender_ipk, d)?;
            Ok(Some(InboundDecoded::PairDeclined))
        },
    }
}

/// Handle an inbound `PairDecline`: verify it's from the pending
/// contact and validly signed (a malicious relay must not forge a rejection to
/// grief a pair), then mark them REJECTED and fail the messages we sent them.
fn process_pair_decline_inbound(sender_ipk: [u8; 32], d: PairDeclineP) -> Result<()> {
    if d.sender_ipk.0 != sender_ipk {
        bail!("pair-decline sender_ipk mismatch");
    }
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    if d.recipient_ipk.0 != our_ipk {
        bail!("pair-decline not addressed to us");
    }
    let vk = VerifyingKey::from_bytes(&sender_ipk).map_err(|e| anyhow!("decliner ipk: {e}"))?;
    let msg = pair_decline_signing_input(&sender_ipk, &our_ipk, d.reason, d.timestamp);
    vk.verify_strict(&msg, &Signature::from_bytes(&d.sig.0))
        .map_err(|_| anyhow!("pair-decline signature invalid"))?;
    if !Contact::exists(&sender_ipk) {
        bail!("pair-decline from non-contact");
    }
    // A live MLS group is ground truth: a decline for some late/redundant
    // handshake must not tear down an already-paired conversation (nor fail its
    // messages). Only a never-completed, group-less pair is rejectable.
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

/// Post-restore self-heal: a known contact sent into a group we hold no
/// state for — mint a fresh group + Welcome toward them so the NEXT
/// messages flow (the mirror of the send path's "no local state;
/// recreating"). The lost ciphertext stays lost (forward secrecy).
///
/// Mint-storm guard: a backlog of N dead-group messages must re-establish
/// once, not N times. Under the per-recipient lock, recreate only while the
/// contact row still points at the dead gid (or none); the first heal
/// repoints it, so the rest of the backlog skips. Best-effort — a failure
/// (e.g. KP stash miss) just waits for the peer's next message to retry.
async fn heal_dead_group<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], dead_gid: &[u8; 32],
) {
    if !Contact::exists(&sender_ipk) {
        return;
    }
    let Some(our_ipk) = Identity::get().map(|i| i.ipk()) else { return };
    let Ok(ipk_signer) = crate::data::identity::secret_key_signing(&our_ipk) else { return };

    let lock = group_create_lock(&sender_ipk);
    let _guard = lock.lock().await;

    let current = Contact::get(&sender_ipk).and_then(|c| c.inner.mls_group_id);
    if !(current.is_none() || current == Some(*dead_gid)) {
        return; // already re-established since this envelope was queued
    }
    match lazy_create_group(ctx, &our_ipk, &ipk_signer, &sender_ipk).await {
        Ok(g) => {
            let _ = Contact::set_mls_group_id(&sender_ipk, &g.group_id());
            info!(
                "MLS: re-established group with {} after dead-group inbound",
                hex::encode(&sender_ipk[..4])
            );
        },
        Err(e) => {
            warn!("MLS: dead-group re-establish with {} failed: {e}", hex::encode(&sender_ipk[..4]))
        },
    }
}

/// Result of [`process_inbound_envelope`] — what kind of payload was
/// processed (UI / metrics consumer).
#[allow(dead_code)]
// ApplicationStale is reachable only when the
// receive path passes a stale message through. The current dispatcher
// folds it into ApplicationBuffered; the explicit stale-drop path is
// future work.
#[derive(Debug)]
pub enum InboundDecoded {
    /// Welcome processed; group activated. The caller probably wants
    /// to emit an "added to group" UI event (future work).
    Welcome,
    /// Application message decrypted. `group_id` names the MLS group it
    /// arrived in, which the caller resolves to a conversation; `author` is
    /// the member who wrote it, taken from the authenticated leaf credential
    /// rather than from whoever handed the envelope to the relay.
    Application {
        plaintext: Vec<u8>,
        group_id:  [u8; 32],
        author:    [u8; 32],
    },
    /// Application message buffered for a future epoch.
    ApplicationBuffered,
    /// Application message stale (epoch < current); dropped.
    ApplicationStale,
    /// Application message whose sender-ratchet secret was already discarded.
    ApplicationUndecryptable,
    /// Application message for a group we hold no local state for (post-
    /// restore, or state loss). The ciphertext is unrecoverable by
    /// construction (FS) — the caller MUST ack so the relay GCs it instead
    /// of redelivering forever; `process_inbound_envelope` fires the
    /// re-establishment toward known contacts (see `heal_dead_group`).
    ApplicationNoGroup { group_id: [u8; 32] },
    /// We couldn't accept a Welcome (KP consumed / group build failed). The
    /// caller sends a `PairDecline` back to the inviter and acks.
    WelcomeRejected { sender_ipk: [u8; 32], reason: u8 },
    /// The invitee declined our pair; already applied (contact REJECTED,
    /// PENDING-era messages failed). Terminal — the caller just acks.
    PairDeclined,
}

/// Outcome of accepting a pairing Welcome. A gate/auth failure is still an
/// `Err` (bogus welcome — bail, no decline); only a *post-gate* accept failure
/// is `Rejected`, which the caller turns into a `PairDecline` back to the
/// inviter.
enum WelcomeOutcome {
    Accepted,
    Rejected(u8),
}

fn process_welcome_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: WelcomeEnvelopeP,
) -> Result<WelcomeOutcome> {
    if env.sender_ipk.0 != sender_ipk {
        bail!("welcome envelope sender_ipk mismatch with DispatchP.from");
    }

    // Defensive check that the envelope is addressed to *us*. The
    // outer `sender_sig` transcript already binds `recipient_ipk`,
    // so a malicious sender cannot address-spoof another recipient.
    // But a delivery-layer bug could deliver an envelope intended for
    // a different device to this one; surface that as a typed error
    // instead of silently activating a group we don't belong to.
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    if env.recipient_ipk.0 != our_ipk {
        warn!(
            "MLS: dropped Welcome addressed to {} (we are {})",
            hex::encode(&env.recipient_ipk.0[..4]),
            hex::encode(&our_ipk[..4])
        );
        bail!("welcome envelope recipient_ipk does not match self");
    }

    // Contact-or-invite gate: accept a Welcome from a stranger only if it
    // carries a valid pairing invite we minted. Capture the name here but DON'T
    // save yet — the save moves after a successful accept so a failed accept
    // leaves no bricked contact (symmetric to the inviter's no-brick fix).
    // Welcomes from existing contacts skip the invite check.
    let (new_contact_name, redeemed_invite) = if Contact::exists(&sender_ipk) {
        (None, None)
    } else {
        let Some(pairing) =
            env.pairing.as_ref().filter(|p| Identity::verify_invite(&p.invite))
        else {
            warn!(
                "MLS: dropped Welcome from unknown sender {} (no valid invite)",
                hex::encode(&sender_ipk[..4])
            );
            bail!("unknown sender and no valid invite");
        };
        (
            Some(pairing.sender_name.chars().take(32).collect::<String>()),
            Some(pairing.invite.clone()),
        )
    };

    // Post-gate accept failure = a decline, not a bail: we were invited, we
    // just couldn't build the group (KP already consumed, malformed welcome).
    let group = match process_welcome_inbound_no_contacts(ctx, sender_ipk, env) {
        Ok(g) => g,
        Err(e) => {
            warn!("MLS: welcome accept failed from {}: {e}", hex::encode(&sender_ipk[..4]));
            return Ok(WelcomeOutcome::Rejected(
                common::proto::mls_wire::DECLINE_GROUP_BUILD_FAILED,
            ));
        },
    };

    // Success: now save the contact (defaults PAIRED) and bind the group.
    if let Some(name) = new_contact_name {
        match Contact::save(sender_ipk, name) {
            Ok(_) => info!("IDENTITY: paired with {}", hex::encode(&sender_ipk[..4])),
            Err(e) => warn!("IDENTITY: failed to save paired contact: {e}"),
        }
        if let Some(invite) = redeemed_invite {
            Identity::spend_invite(&invite);
        }
    }
    let _ = Contact::set_mls_group_id(&sender_ipk, &group.group_id());

    info!(
        "MLS: welcome from {} activated group {}",
        hex::encode(&sender_ipk[..4]),
        hex::encode(&group.group_id()[..4])
    );
    Ok(WelcomeOutcome::Accepted)
}

/// Globals-free variant of [`process_welcome_inbound`].
/// Performs the outer-sig verify, openmls processing, and stash
/// `on_consumed` bookkeeping but bypasses the
/// [`Contact::exists`]/[`Contact::set_mls_group_id`] writes that
/// reach into the libcore-global SQLite tables. Returns the
/// freshly-built [`MlsGroupHandle`] so callers can persist the
/// group-id mapping in their own state.
///
/// **Auth posture**: still rejects sender_ipk-mismatch and any sig
/// failure; relies on the caller having checked sender identity at a
/// transport layer (in production: contact-first gate; in tests:
/// per-client identity store).
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

fn process_application_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: MlsApplicationEnvelopeP,
    accepted_at_ms: u64,
) -> Result<InboundDecoded> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    process_application_inbound_for(ctx, sender_ipk, &our_ipk, env, accepted_at_ms)
}

/// Persist messages drained from the epoch-ahead buffer. These were
/// `let _ =`-discarded before — every catch-up message silently lost
/// once its epoch became current.
///
/// Each message attributes to the author on its own MLS leaf credential, so a
/// message that waited out several epochs is still credited to whoever wrote
/// it rather than to whoever's arrival unblocked the queue.
///
/// ponytail: `m.dispatch_id` is the buffer's blake3(mls) key (push site
/// below), not the sender's authoritative DispatchP.id: it dedups fine but
/// won't sort by send-time, so delivery watermarks must thread the real id to
/// the push before relying on ordering.
fn persist_drained(
    drained: Vec<crate::mls::epoch_catchup::ProcessedApplicationMessage>, conversation: [u8; 16],
    fallback_sender: [u8; 32],
) {
    for m in drained {
        let sender_ipk = m.sender.unwrap_or(fallback_sender);
        let Ok(did): Result<[u8; 16], _> = m.dispatch_id.as_slice().try_into() else { continue };
        let ts = crate::quic::server::accepted_at_secs(m.accepted_at_ms);
        // Post carries the quote target alongside the body; pre-v12 payloads
        // reach the same persist through legacy_body.
        let parsed = match AppPayload::deser(&m.plaintext) {
            Ok(AppPayload::Post { reply_to, body }) => Some((reply_to, body)),
            Ok(p) => legacy_body(p),
            Err(_) => None,
        };
        let Some((reply_to, body)) = parsed else { continue };
        match save_inbound_body(&conversation, &sender_ipk, &did, ts, reply_to, body) {
            Ok(Some((saved, content))) => MessageEv::Received {
                id: saved.inner.id,
                conversation,
                sender: sender_ipk,
                content,
                timestamp: ts,
            }
            .emit(),
            Ok(None) => {},
            Err(e) => warn!("MESSAGE: drained persist failed: {e}"),
        }
    }
}

/// Explicit-recipient variant of [`process_application_inbound`].
/// Identical pipeline but takes `our_ipk` as an argument instead of
/// reading the global `Identity::get()`. Production
/// `process_application_inbound` delegates here. The e2e harness uses
/// this directly so each test client can assert against its own IPK
/// without sharing a process-global identity row.
pub fn process_application_inbound_for<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], our_ipk: &[u8; 32], env: MlsApplicationEnvelopeP,
    accepted_at_ms: u64,
) -> Result<InboundDecoded> {
    // 1. Outer envelope sig verifies under sender's IPK.
    let transcript = envelope_signing_input(
        PROTOCOL_VERSION,
        our_ipk,
        &env.group_id.0,
        env.epoch,
        &env.mls_message.0,
    );
    let vk = VerifyingKey::from_bytes(&sender_ipk)
        .map_err(|e| anyhow!("sender_ipk not Ed25519: {e}"))?;
    let sig = Signature::from_bytes(&env.sender_sig.0);
    // `verify_strict` for non-canonical-sig rejection.
    vk.verify_strict(&transcript, &sig).map_err(|_| anyhow!("application envelope sig invalid"))?;

    // 2. Look up the group. No local state (post-restore / state loss) is a typed outcome, not an
    //    error: an `Err` here meant the live path never acked and the relay redelivered the same
    //    doomed envelope forever.
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

    // 3. Compare epoch.
    let current = group.epoch();
    if env.epoch > current {
        // Refuse to buffer envelopes more than `MAX_EPOCH_AHEAD`
        // epochs in the future. The recipient
        // realistically cannot catch up that far without seeing every
        // intermediate commit (the buffer would just hold dead bytes
        // until eviction); a malicious member otherwise pins memory
        // at `MAX_PER_GROUP × MAX_EPOCH_AHEAD` for free.
        let delta = env.epoch - current;
        if delta > common::proto::mls_wire::MAX_EPOCH_AHEAD {
            warn!(
                "MLS: dropping far-future envelope (delta={} > MAX_EPOCH_AHEAD={}) for group {}",
                delta,
                common::proto::mls_wire::MAX_EPOCH_AHEAD,
                hex::encode(&env.group_id.0[..4])
            );
            bail!(
                "envelope epoch {} too far ahead of current {} (delta {} > MAX_EPOCH_AHEAD {})",
                env.epoch,
                current,
                delta,
                common::proto::mls_wire::MAX_EPOCH_AHEAD
            );
        }
        // Buffer for catchup later.
        let dispatch_id = blake3::hash(&env.mls_message.0).as_bytes()[..16].to_vec();
        ctx.buffer
            .push(&group, env.mls_message.0.clone(), env.epoch, dispatch_id, accepted_at_ms)
            .map_err(|e| anyhow!("epoch-ahead buffer push: {e}"))?;
        return Ok(InboundDecoded::ApplicationBuffered);
    }
    if env.epoch < current {
        // Explicitly drop stale-epoch envelopes rather than falling
        // through to `process_incoming` (which errors on
        // stale, which makes `handle_deliver` refuse to ack, which
        // makes the relay redeliver forever — unbounded queue + CPU).
        // The caller in `handle_deliver` ack's on `ApplicationStale`
        // so the relay GCs the message.
        warn!(
            "MLS: dropping stale-epoch envelope (env={} < current={}) for group {}",
            env.epoch,
            current,
            hex::encode(&env.group_id.0[..4])
        );
        return Ok(InboundDecoded::ApplicationStale);
    }

    use openmls::prelude::tls_codec::Deserialize as _;
    let in_msg = openmls::prelude::MlsMessageIn::tls_deserialize_exact(&env.mls_message.0)
        .map_err(|e| anyhow!("MlsMessageIn deser: {e:?}"))?;
    let proto: ProtocolMessage =
        in_msg.try_into_protocol_message().map_err(|e| anyhow!("not a ProtocolMessage: {e:?}"))?;

    let processed = match group.process_incoming(ctx.provider, proto) {
        Ok(processed) => processed,
        Err(err) if err.is_spent_secret() => return Ok(InboundDecoded::ApplicationUndecryptable),
        Err(err) => return Err(anyhow!("process_incoming: {err}")),
    };

    // The MLS leaf credential is the authority on who wrote this; the outer
    // envelope sender only proves who put it on the wire. They coincide in a
    // 1:1 chat and routinely diverge in a group.
    let author = processed.sender.unwrap_or(sender_ipk);
    match processed.content {
        ProcessedMessageContent::ApplicationMessage(app) => {
            let plaintext = app.into_bytes();
            // After every successful processing, drain any buffered
            // ahead-of-epoch messages and persist them (not discard).
            persist_drained(
                ctx.buffer.drain_when_ready(&mut group, ctx.provider).unwrap_or_default(),
                Conversation::for_group(&env.group_id.0).unwrap_or_default(),
                author,
            );
            Ok(InboundDecoded::Application { plaintext, group_id: env.group_id.0, author })
        },
        ProcessedMessageContent::StagedCommitMessage(staged) => {
            let roster = group.member_count() + staged.add_proposals().count();
            if roster > crate::mls::MAX_GROUP_MEMBERS {
                return Err(anyhow!(
                    "commit would take the group to {roster} members, limit is {}",
                    crate::mls::MAX_GROUP_MEMBERS
                ));
            }
            group
                .merge_staged_commit(ctx.provider, *staged)
                .map_err(|e| anyhow!("merge_staged_commit: {e}"))?;
            // The merged commit is the authority on who is in the group now,
            // so re-read the roster from it rather than trusting the narration
            // that accompanies it. A member removed here keeps their row,
            // marked inactive, so their old messages still resolve to a name.
            if let Some(conversation) = Conversation::for_group(&env.group_id.0) {
                let roster: Vec<[u8; 32]> = group
                    .members()
                    .filter_map(|m| m.credential.serialized_content().try_into().ok())
                    .collect();
                if let Err(e) = Conversation::sync_roster(&conversation, &roster) {
                    warn!("GROUP: could not sync the roster after a commit: {e}");
                }
            }
            // After commit-merge, drain any newly-processable buffered
            // messages and persist them (not discard).
            persist_drained(
                ctx.buffer.drain_when_ready(&mut group, ctx.provider).unwrap_or_default(),
                Conversation::for_group(&env.group_id.0).unwrap_or_default(),
                author,
            );
            Ok(InboundDecoded::ApplicationBuffered)
        },
        ProcessedMessageContent::ProposalMessage(_)
        | ProcessedMessageContent::ExternalJoinProposalMessage(_) => {
            // Proposals are handled by the next commit; no application
            // message to surface.
            Ok(InboundDecoded::ApplicationBuffered)
        },
    }
}

/// Maximum re-fetch attempts for a Welcome that fails signature
/// verification or comes from an unknown contact. After this many
/// reconnect-cycles, the welcome is acked anyway (and dropped) to
/// prevent the queue from growing forever on a persistently-bad
/// sender. Preserves evidence the future anti-abuse spec needs while
/// still bounding queue size.
const POLL_WELCOMES_MAX_RETRY: u8 = 5;

/// In-process retry-count tracker for [`poll_welcomes`]. Maps
/// `welcome_id → attempts_so_far`. Lives across calls to `poll_welcomes`
/// within one process; not persisted across restarts (after a restart
/// the home queue's natural TTL still bounds growth, and the user's
/// reconnect cadence resets the counter).
///
/// A minimal in-memory map (vs. a SQLite table) keeps the design
/// surface small; the trade-off is that the retry count resets on
/// libcore restart. Acceptable: a malicious sender would have
/// to keep re-publishing across the user's full reconnect cycle to
/// keep their slot, while the home's TTL caps the queue regardless.
static WELCOME_RETRY_COUNTS: once_cell::sync::Lazy<parking_lot::Mutex<HashMap<[u8; 8], u8>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Drain pending Welcomes from the K=3 homes of our IPK. Run once on
/// every reconnect.
///
/// Returns the count of welcomes successfully processed.
///
/// Distinguishes three outcome classes per welcome:
///   - **Success**: ack immediately (the welcome did its job).
///   - **Known sender, bad processing**: ack (a known contact sending us malformed welcomes is a
///     contact-flow bug, not abuse).
///   - **Unknown sender or bad sig**: hold without acking; bump a per-`welcome_id` retry counter.
///     Ack-and-drop only after `POLL_WELCOMES_MAX_RETRY` failed attempts. This preserves the
///     evidence the future anti-abuse spec (HANDOFF priority 2) wants while still bounding queue
///     growth.
#[allow(dead_code)] // The production call site is wired in quic/server.
pub async fn poll_welcomes<C: DhtClient>(ctx: &MlsContext<'_, C>) -> Result<usize> {
    let entries = ctx.dht.fetch_welcomes().await.map_err(|e| anyhow!("fetch_welcomes: {e}"))?;

    let mut ack_ids: Vec<[u8; 8]> = Vec::with_capacity(entries.len());
    let mut count = 0usize;
    for entry in entries {
        // The wire-level receive path normally goes through
        // `process_inbound_envelope` with the DispatchP.from acting
        // as the authenticated sender_ipk. For a Welcome drained from
        // the queue, the envelope's `sender_ipk` field IS the source
        // of truth (the home countersigns nothing of ours). We
        // therefore pass `envelope.sender_ipk` as the trusted sender
        // — process_welcome's outer-sig verification under that IPK
        // is the authentication.
        let sender_ipk = entry.envelope.sender_ipk.0;
        let welcome_id = entry.welcome_id.0;
        let known_contact = Contact::exists(&sender_ipk);
        match process_welcome_inbound(ctx, sender_ipk, entry.envelope) {
            Ok(WelcomeOutcome::Accepted) => {
                ack_ids.push(welcome_id);
                count += 1;
                WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
                // Prove the pair to the inviter. This drain path bypasses
                // process_deliver, so the ack must fire here too — otherwise an
                // offline-received pair never confirms.
                crate::RUNTIME.spawn(async move {
                    let _ = send_pair_ack(sender_ipk).await;
                });
            },
            Ok(WelcomeOutcome::Rejected(reason)) => {
                // Couldn't accept — tell the inviter and ack (re-fetch won't help).
                crate::RUNTIME.spawn(async move {
                    let _ = send_pair_decline(sender_ipk, reason).await;
                });
                ack_ids.push(welcome_id);
                WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
            },
            Err(e) => {
                if known_contact {
                    // Known contact, but processing failed — could be
                    // a contact-flow bug, a bad sig from a compromised
                    // device, etc. Log + ack (we won't make progress
                    // by re-fetching the same bytes).
                    log::warn!(
                        "MLS: poll_welcomes: drop bad welcome from known contact {}: {e}",
                        hex::encode(&sender_ipk[..4])
                    );
                    ack_ids.push(welcome_id);
                    WELCOME_RETRY_COUNTS.lock().remove(&welcome_id);
                } else {
                    // Unknown sender / bad sig: hold for re-evaluation
                    // on the next reconnect (sender may be added as a
                    // contact in the meantime; bad-sig may be a
                    // transient corruption). Bump the retry count;
                    // drop only after POLL_WELCOMES_MAX_RETRY attempts.
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

// -----------------------------------------------------------
// Backwards-compat helpers retained for the v2 `decode_encrypted`
// path used by `quic/server.rs::handle_deliver`. The receive path
// is rewritten further below; until that lands the caller uses
// `process_inbound_envelope` directly.
// -----------------------------------------------------------

// `decode_encrypted` was dropped — the caller in
// `quic/server.rs::handle_deliver` now uses `process_inbound_envelope`.

// Read paths (get_messages / get_conversations / get_contacts) live in the
// uniffi translation layer `api::messaging`, calling the engine's
// `Message::get_*` / `Contact::list` directly — no CBOR round-trip.

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! In-process send/receive tests. These do **not** drive the
    //! global `RELAY` connection — that path is owned by the e2e
    //! harness. We exercise the MLS-layer pipeline directly:
    //! lazy-create, application encrypt, envelope sign, then a
    //! co-resident receiver decodes the envelope and decrypts.

    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use ed25519_dalek::Verifier;
    use parking_lot::Mutex;
    use rusqlite::Connection;

    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use crate::quic::dht_client::tests::FakeDhtClient;

    /// Open a fresh in-memory MLS DB and wrap it in the shared
    /// connection handle the stash + provider both consume.
    fn fresh_mls_conn() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        Arc::new(Mutex::new(conn))
    }

    /// One libcore-flavoured "node" — a distinct openmls provider +
    /// stash + epoch buffer + IPK keypair, as if it were a separate
    /// device. Sharing the *same* `FakeDhtClient` arc across nodes is
    /// the in-process equivalent of "they reach the same DHT".
    struct Node {
        ipk_signer: SigningKey,
        ipk:        [u8; 32],
        provider:   PromtuzMlsProvider,
        stash:      KeyPackageStash,
        buffer:     EpochCatchupBuffer,
    }

    impl Node {
        fn new(seed: u8) -> Self {
            let ipk_signer = SigningKey::from_bytes(&[seed; 32]);
            let ipk = ipk_signer.verifying_key().to_bytes();
            let conn = fresh_mls_conn();
            let provider = PromtuzMlsProvider::new(conn.clone());
            let stash = KeyPackageStash::new(conn.clone());
            let buffer = EpochCatchupBuffer::new(conn);
            Self { ipk_signer, ipk, provider, stash, buffer }
        }

        fn ctx<'a, C: DhtClient>(&'a self, dht: &'a C) -> MlsContext<'a, C> {
            MlsContext { provider: &self.provider, stash: &self.stash, buffer: &self.buffer, dht }
        }
    }

    /// Lazy-create flow end-to-end: founder publishes a Welcome via
    /// the fake dialer, recipient activates the group via the
    /// lower-level `process_welcome` (we bypass
    /// `process_welcome_inbound` because its contact-first gate
    /// touches the production CONTACTS_DB lazy static, unsuitable for
    /// in-process unit tests).
    #[tokio::test(flavor = "current_thread")]
    async fn lazy_create_publishes_welcome_via_dialer() {
        let alice = Node::new(0x11);
        let bob = Node::new(0x22);
        let dht = FakeDhtClient::new_arc();

        // Bob mints + publishes one KP; the fake dialer treats the
        // published_kp slot as the "stash" Alice fetches from.
        let bob_kps = bob
            .stash
            .ensure_stash_full(&bob.provider, &bob.ipk_signer)
            .expect("bob ensure_stash_full");
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .expect("seed bob's published kps");

        // Alice runs lazy_create_group.
        let g =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group");

        // Group is at epoch 1 (alice + bob added).
        assert_eq!(g.epoch(), 1);
        assert_eq!(g.member_count(), 2);

        // The fake recorded a published Welcome.
        let entries = dht.fetch_welcomes().await.expect("fetch_welcomes");
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap();

        // Bob materialises the group via the low-level `process_welcome`.
        let bob_group = process_welcome(&bob.provider, &entry.envelope).expect("process welcome");
        assert_eq!(bob_group.epoch(), g.epoch());
        assert_eq!(bob_group.member_count(), g.member_count());
        assert_eq!(bob_group.group_id(), g.group_id());
    }

    /// The retry-path guarantee: a pending outgoing Image, when its payload is
    /// rebuilt for resend, comes back as `AppPayload::Image` carrying the AVIF
    /// blob — NOT a bare-caption `Text`. This is the send-side content-loss the
    /// media-aware rebuild closes; a peer whose KeyPackage was late would
    /// otherwise receive the caption with no picture. Drives the real
    /// process-global `MESSAGES_DB` (build_image_message + media::get), so
    /// point it at a scratch dir first.
    #[test]
    fn deferred_image_resends_as_image_not_text() {
        let dir = std::env::temp_dir().join("promtuz-deferred-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let to = [0x51u8; 16];
        let avif = vec![7u8, 8, 9, 10];
        let msg = build_image_message(to, 4, 3, "look at this", None).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();

        // Un-finalized placeholder: a reconnect retry mid-compress must NOT
        // send an empty-blob Image — it bails and the row stays pending.
        assert!(rebuild_pending_payload(&to, &msg).is_err(), "placeholder must not rebuild");

        crate::data::media::set_blob(&to, &did, &avif, 4, 3).unwrap();

        // The rebuilt payload a resend would encrypt: must be the whole Image.
        let bytes = rebuild_pending_payload(&to, &msg).unwrap();
        match AppPayload::deser(&bytes).unwrap() {
            AppPayload::Post {
                reply_to: None,
                body: Body::Image { caption, data, width, height, mime, .. },
            } => {
                assert_eq!(caption, "look at this");
                assert_eq!(data, avif, "the AVIF blob must ride the resend");
                assert_eq!((width, height), (4, 3));
                assert_eq!(mime, "image/avif");
            },
            other => panic!("deferred image must rebuild as a Post carrying Image, got {other:?}"),
        }

        // A plain text row (no media side-row) still rebuilds as Text.
        let tmsg = Message::save_outgoing(to, "hi", None).unwrap();
        let tbytes = rebuild_pending_payload(&to, &tmsg).unwrap();
        assert!(
            matches!(
                AppPayload::deser(&tbytes).unwrap(),
                AppPayload::Post { reply_to: None, body: Body::Text(t) } if t == "hi"
            ),
            "a text row must not be dressed up as media",
        );
    }

    /// Every cell of the revision matrix. Text and image both ride inside the
    /// frame the peer already holds, so those interchange; an attachment is
    /// fetched device-to-device and a sticker is atomic, so each stays on its
    /// own diagonal. Exhaustive rather than sampled — the refusals are the
    /// half that protects an in-flight transfer.
    #[test]
    fn revision_matrix_permits_inline_swaps_only() {
        use BodyKind::Attachment;
        use BodyKind::Image;
        use BodyKind::Sticker;
        use BodyKind::Text;

        let allowed = [
            (Text, Text),
            (Text, Image),
            (Image, Text),
            (Image, Image),
            (Attachment, Attachment),
            (Sticker, Sticker),
        ];
        for (from, to) in [Text, Image, Attachment, Sticker]
            .into_iter()
            .flat_map(|f| [Text, Image, Attachment, Sticker].into_iter().map(move |t| (f, t)))
        {
            let want = allowed.contains(&(from, to));
            assert_eq!(
                from.revisable_to(to),
                want,
                "{from:?} -> {to:?} should be {}",
                if want { "allowed" } else { "refused" },
            );
        }
    }

    /// A quote target rides the Post envelope rather than the body, so it
    /// survives on media. Shares the scratch dir above: `MESSAGES_DB` is
    /// process-global and initialises once, whichever test reaches it first.
    #[test]
    fn a_media_row_rebuilds_as_a_post_that_keeps_its_quote() {
        let dir = std::env::temp_dir().join("promtuz-deferred-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let to = [0x52u8; 16];
        let quoted = [0x77u8; 16];
        let media = crate::data::media::MediaRow {
            kind: crate::data::media::KIND_IMAGE,
            group_id: None,
            mime: "image/avif".into(),
            name: String::new(),
            size: 3,
            width: 2,
            height: 2,
            blob: Some(vec![1, 2, 3]),
            thumb: None,
            file_id: None,
        };
        let msg =
            crate::data::media::save_outgoing_with_media(&to, "cap", Some(quoted), &media).unwrap();

        let bytes = rebuild_pending_payload(&to, &msg).unwrap();
        assert!(
            matches!(
                AppPayload::deser(&bytes).unwrap(),
                AppPayload::Post { reply_to: Some(rt), body: Body::Image { .. } } if rt == quoted
            ),
            "a replying image must rebuild as a Post carrying both the quote and the picture",
        );
    }

    /// The defer predicate `attempt_send` relies on: a lazy-create that
    /// fails because the peer never published a KeyPackage must surface a
    /// `DhtClientError::NoStash` reachable through the anyhow chain
    /// (Change 1 — we must NOT stringify it). This is the exact condition
    /// that leaves a first-send PENDING instead of hard-failing, and the
    /// negative case guards against blanket-defer.
    #[tokio::test(flavor = "current_thread")]
    async fn lazy_create_preserves_nostash_for_defer_detection() {
        let alice = Node::new(0xB1);
        let bob = Node::new(0xB2);
        let dht = FakeDhtClient::new_arc();
        // NB: bob never publishes a KP → fetch_keypackage_for → NoStash.

        let err =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect_err("no published KP must fail lazy_create");

        let is_nostash = err
            .chain()
            .any(|c| matches!(c.downcast_ref::<DhtClientError>(), Some(DhtClientError::NoStash)));
        assert!(is_nostash, "NoStash must be downcastable for the defer path; got: {err:?}");

        // Blanket-defer guard: an unrelated error must NOT match.
        let other = anyhow!("some unrelated failure");
        assert!(
            !other.chain().any(|c| matches!(
                c.downcast_ref::<DhtClientError>(),
                Some(DhtClientError::NoStash)
            )),
            "non-NoStash errors must stay a hard fail"
        );
    }

    /// Application message round-trip: alice sends, bob receives.
    /// Drives `build_application_envelope_bytes` → outer sig
    /// verification → openmls decrypt — the production
    /// `process_application_inbound` is bypassed because its outer
    /// sig check requires a live Identity::get() against the
    /// production IDENTITY_DB.
    #[tokio::test(flavor = "current_thread")]
    async fn application_round_trip_through_envelope_pipeline() {
        let alice = Node::new(0x55);
        let bob = Node::new(0x66);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        // Alice creates the group + Welcome; Bob materialises.
        let mut alice_group =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");

        // Alice builds an envelope.
        let plaintext = b"hello bob, this is alice on MLS";
        let leaf_kp = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let ctx_a = alice.ctx(dht.as_ref());
        let payload = build_application_envelope_bytes(
            &ctx_a,
            &mut alice_group,
            &leaf_kp,
            &alice.ipk,
            &bob.ipk,
            plaintext,
            &alice.ipk_signer,
        )
        .expect("build envelope");

        // Bob decodes the envelope.
        let env_outer = MlsEnvelopeP::deser(&payload).expect("decode envelope");
        let env_app = match env_outer {
            MlsEnvelopeP::Application(a) => a,
            other => panic!("expected Application, got {other:?}"),
        };

        // Verify outer sig under alice's IPK (`to_ipk = bob.ipk` bound
        // in the transcript).
        let transcript = envelope_signing_input(
            PROTOCOL_VERSION,
            &bob.ipk,
            &alice_group.group_id(),
            alice_group.epoch(),
            &env_app.mls_message.0,
        );
        let vk = VerifyingKey::from_bytes(&alice.ipk).expect("vk");
        let sig = Signature::from_bytes(&env_app.sender_sig.0);
        vk.verify(&transcript, &sig).expect("outer sig verifies");

        // Decode + decrypt with Bob's group state.
        let mut bob_group =
            MlsGroupHandle::load(&bob.provider, &alice_group.group_id()).unwrap().unwrap();
        use openmls::prelude::tls_codec::Deserialize as _;
        let in_msg =
            openmls::prelude::MlsMessageIn::tls_deserialize_exact(&env_app.mls_message.0).unwrap();
        let proto: ProtocolMessage = in_msg.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, proto).unwrap();
        assert_eq!(
            processed.sender,
            Some(alice.ipk),
            "the leaf credential names the author, not the envelope carrier"
        );
        match processed.content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext);
            },
            other => panic!("expected ApplicationMessage, got {other:?}"),
        }
    }

    /// Epoch-ahead message gets buffered, then drained on commit-merge.
    #[tokio::test(flavor = "current_thread")]
    async fn epoch_ahead_application_buffers_then_drains_after_commit_merge() {
        let alice = Node::new(0x77);
        let bob = Node::new(0x88);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        let mut alice_group =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");
        let mut bob_group =
            MlsGroupHandle::load(&bob.provider, &alice_group.group_id()).unwrap().unwrap();

        // Alice encrypts an Application at the current epoch. We
        // shove it into Bob's buffer at `current_epoch`; the drain
        // accepts `epoch <= current` and processes immediately.
        let alice_leaf = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let plaintext = b"buffered then drained";
        let mls_msg = alice_group
            .create_application_message(&alice.provider, &alice_leaf, plaintext)
            .unwrap();
        let mls_bytes =
            openmls::prelude::tls_codec::Serialize::tls_serialize_detached(&mls_msg).unwrap();

        let dispatch_id = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
        let outcome = bob
            .buffer
            .push(&bob_group, mls_bytes, alice_group.epoch(), dispatch_id.clone(), 0)
            .expect("push");
        assert_eq!(outcome, crate::mls::PushOutcome::Inserted);

        let drained = bob.buffer.drain_when_ready(&mut bob_group, &bob.provider).expect("drain");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].plaintext, plaintext);
    }

    /// `send_message_inner`-side surface check: lazy_create_group
    /// invariants — published exactly one Welcome, addressed to the
    /// peer, sender_ipk == alice.
    #[tokio::test(flavor = "current_thread")]
    async fn send_message_inner_lazy_creates_group_when_contact_has_none() {
        let alice = Node::new(0xA1);
        let bob = Node::new(0xA2);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        let g =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group");

        let pubs = dht.welcomes_published.lock();
        assert_eq!(pubs.len(), 1);
        let env = &pubs[0];
        assert_eq!(env.recipient_ipk.0, bob.ipk);
        assert_eq!(env.sender_ipk.0, alice.ipk);
        assert_eq!(env.group_id.0, g.group_id());
    }

    /// Two concurrent acquisitions of
    /// `group_create_lock(&recipient_ipk)` must serialise. The lock is
    /// the load-bearing primitive `send_message_inner` uses to defeat
    /// the duplicate-group race; here we exercise it directly.
    ///
    /// We don't drive the full `lazy_create_group` from two tasks
    /// concurrently because the in-process MLS provider's RocksDB
    /// connection is mutex-locked anyway and would serialise the
    /// fetch+publish under the hood. Instead we record observed
    /// "lock held" intervals per task and assert non-overlap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn group_create_lock_serialises_per_recipient_ipk() {
        let recipient: [u8; 32] = [0xC2; 32];
        let other: [u8; 32] = [0xC9; 32];

        // Distinct recipients → distinct locks → no contention.
        let lock_a = group_create_lock(&recipient);
        let lock_b = group_create_lock(&other);
        assert!(!Arc::ptr_eq(&lock_a, &lock_b), "distinct ipks → distinct locks");

        // Same recipient → same lock instance.
        let lock_a2 = group_create_lock(&recipient);
        assert!(Arc::ptr_eq(&lock_a, &lock_a2), "same ipk → same lock instance");

        // Two tasks contending the same lock: the second must wait.
        let in_critical = Arc::new(parking_lot::Mutex::new(0u32));
        let max_observed = Arc::new(parking_lot::Mutex::new(0u32));

        let lock_a_h1 = lock_a.clone();
        let in_h1 = in_critical.clone();
        let max_h1 = max_observed.clone();
        let h1 = tokio::spawn(async move {
            let _g = lock_a_h1.lock().await;
            *in_h1.lock() += 1;
            {
                let cur = *in_h1.lock();
                let mut m = max_h1.lock();
                if cur > *m {
                    *m = cur;
                }
            }
            // Hold for a few yields so the racing task definitely
            // tries to acquire.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            *in_h1.lock() -= 1;
        });

        let lock_a_h2 = lock_a.clone();
        let in_h2 = in_critical.clone();
        let max_h2 = max_observed.clone();
        let h2 = tokio::spawn(async move {
            let _g = lock_a_h2.lock().await;
            *in_h2.lock() += 1;
            {
                let cur = *in_h2.lock();
                let mut m = max_h2.lock();
                if cur > *m {
                    *m = cur;
                }
            }
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            *in_h2.lock() -= 1;
        });

        h1.await.unwrap();
        h2.await.unwrap();

        assert_eq!(
            *max_observed.lock(),
            1,
            "per-recipient lock must serialise: at most one task in critical section"
        );
    }

    /// An Application envelope with `epoch < current` is dropped
    /// explicitly with `ApplicationStale`, NOT
    /// passed through to openmls (which would error and force the
    /// relay to redeliver indefinitely). The buffer must not grow.
    #[tokio::test(flavor = "current_thread")]
    async fn stale_epoch_application_drops_with_application_stale() {
        let alice = Node::new(0x71);
        let bob = Node::new(0x72);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        let mut alice_group =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");

        // Force bob_group to advance one epoch via Update so its epoch
        // is strictly greater than what alice will sign in the test.
        // Easier: build an envelope claiming epoch 0 and feed it to
        // a freshly-loaded bob_group whose epoch is 1.
        let bob_group =
            MlsGroupHandle::load(&bob.provider, &alice_group.group_id()).unwrap().unwrap();
        assert!(bob_group.epoch() >= 1, "fresh group is at epoch >= 1");

        // Build an envelope at epoch 0 (stale by design).
        let alice_leaf = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let mls_msg = alice_group
            .create_application_message(&alice.provider, &alice_leaf, b"stale msg")
            .unwrap();
        let mls_bytes =
            openmls::prelude::tls_codec::Serialize::tls_serialize_detached(&mls_msg).unwrap();

        use ed25519_dalek::Signer;
        let transcript = envelope_signing_input(
            PROTOCOL_VERSION,
            &bob.ipk,
            &alice_group.group_id(),
            0u64, // stale epoch
            &mls_bytes,
        );
        let outer_sig = alice.ipk_signer.sign(&transcript);

        let env = MlsApplicationEnvelopeP {
            version:     MLS_ENVELOPE_VERSION,
            group_id:    alice_group.group_id().into(),
            epoch:       0u64,
            mls_message: ByteVec(mls_bytes),
            sender_sig:  outer_sig.to_bytes().into(),
        };

        let buffered_before = bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        let result =
            process_application_inbound_for(&bob.ctx(dht.as_ref()), alice.ipk, &bob.ipk, env, 0)
                .expect("stale envelope returns ApplicationStale, not Err");

        match result {
            InboundDecoded::ApplicationStale => {},
            other => panic!("expected ApplicationStale, got {other:?}"),
        }

        let buffered_after = bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        assert_eq!(buffered_before, buffered_after, "stale envelope must not grow the buffer");
    }

    /// An Application envelope for a group the recipient holds NO state
    /// for (post-restore) returns `ApplicationNoGroup`, not `Err` — an
    /// `Err` meant `handle_deliver` never acked and the relay redelivered
    /// the same doomed envelope forever.
    #[tokio::test(flavor = "current_thread")]
    async fn dead_group_application_returns_typed_outcome_not_err() {
        let alice = Node::new(0x91);
        let bob = Node::new(0x92);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        let mut alice_group =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        // Bob never processes the Welcome — his provider holds no state for
        // this group, exactly the post-restore shape.

        let alice_leaf = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let mls_msg = alice_group
            .create_application_message(&alice.provider, &alice_leaf, b"into the void")
            .unwrap();
        let mls_bytes =
            openmls::prelude::tls_codec::Serialize::tls_serialize_detached(&mls_msg).unwrap();

        use ed25519_dalek::Signer;
        let epoch = alice_group.epoch();
        let transcript = envelope_signing_input(
            PROTOCOL_VERSION,
            &bob.ipk,
            &alice_group.group_id(),
            epoch,
            &mls_bytes,
        );
        let outer_sig = alice.ipk_signer.sign(&transcript);

        let env = MlsApplicationEnvelopeP {
            version: MLS_ENVELOPE_VERSION,
            group_id: alice_group.group_id().into(),
            epoch,
            mls_message: ByteVec(mls_bytes),
            sender_sig: outer_sig.to_bytes().into(),
        };

        let result =
            process_application_inbound_for(&bob.ctx(dht.as_ref()), alice.ipk, &bob.ipk, env, 0)
                .expect("dead-group envelope must be a typed outcome, not Err");

        match result {
            InboundDecoded::ApplicationNoGroup { group_id } => {
                assert_eq!(group_id, alice_group.group_id(), "must surface the dead gid");
            },
            other => panic!("expected ApplicationNoGroup, got {other:?}"),
        }
    }

    /// An Application envelope claiming an epoch > current +
    /// MAX_EPOCH_AHEAD is dropped (Err, not buffered) so
    /// a malicious sender can't pin per-recipient memory by shipping
    /// far-future epochs.
    #[tokio::test(flavor = "current_thread")]
    async fn far_future_epoch_application_rejected_not_buffered() {
        use common::proto::mls_wire::MAX_EPOCH_AHEAD;

        let alice = Node::new(0x81);
        let bob = Node::new(0x82);
        let dht = FakeDhtClient::new_arc();

        let bob_kps = bob.stash.ensure_stash_full(&bob.provider, &bob.ipk_signer).unwrap();
        dht.publish_keypackages(&bob_kps[..1], crate::quic::dht_client::KpOutcomeFilter::Default)
            .await
            .unwrap();

        let mut alice_group =
            lazy_create_group(&alice.ctx(dht.as_ref()), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");

        let alice_leaf = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let mls_msg = alice_group
            .create_application_message(&alice.provider, &alice_leaf, b"far future")
            .unwrap();
        let mls_bytes =
            openmls::prelude::tls_codec::Serialize::tls_serialize_detached(&mls_msg).unwrap();

        let bob_group =
            MlsGroupHandle::load(&bob.provider, &alice_group.group_id()).unwrap().unwrap();
        let bad_epoch = bob_group.epoch() + MAX_EPOCH_AHEAD + 1;

        use ed25519_dalek::Signer;
        let transcript = envelope_signing_input(
            PROTOCOL_VERSION,
            &bob.ipk,
            &alice_group.group_id(),
            bad_epoch,
            &mls_bytes,
        );
        let outer_sig = alice.ipk_signer.sign(&transcript);
        let env = MlsApplicationEnvelopeP {
            version:     MLS_ENVELOPE_VERSION,
            group_id:    alice_group.group_id().into(),
            epoch:       bad_epoch,
            mls_message: ByteVec(mls_bytes),
            sender_sig:  outer_sig.to_bytes().into(),
        };

        let buffered_before = bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        let result =
            process_application_inbound_for(&bob.ctx(dht.as_ref()), alice.ipk, &bob.ipk, env, 0);
        assert!(result.is_err(), "far-future envelope must be rejected");
        let buffered_after = bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        assert_eq!(buffered_before, buffered_after, "far-future envelope must not grow the buffer");
    }

    /// Oversize Welcome blob is rejected at the envelope decode
    /// boundary, before openmls sees it.
    #[tokio::test(flavor = "current_thread")]
    async fn oversize_welcome_blob_rejected_at_decode() {
        use common::proto::mls_wire::MAX_WELCOME_BYTES;
        use common::types::bytes::ByteVec;

        let bob = Node::new(0x91);
        let dht = FakeDhtClient::new_arc();

        // Build an envelope with a deliberately-oversize welcome_blob
        // (cap + 1). The envelope sig isn't valid (the blob is junk)
        // but the size cap fires before sig verification.
        let env = WelcomeEnvelopeP {
            version:       MLS_ENVELOPE_VERSION,
            group_id:      [0u8; 32].into(),
            sender_ipk:    [0u8; 32].into(),
            recipient_ipk: bob.ipk.into(),
            welcome_blob:  ByteVec(vec![0u8; MAX_WELCOME_BYTES + 1]),
            kp_ref_used:   [0u8; 32].into(),
            sender_sig:    [0u8; 64].into(),
            pairing:       None,
        };
        let outer = MlsEnvelopeP::Welcome(env);
        let bytes = outer.ser().expect("ser");

        let r = process_inbound_envelope(&bob.ctx(dht.as_ref()), [0u8; 32], &bytes, 0).await;
        assert!(r.is_err(), "oversize welcome must be rejected at decode");
        let msg = format!("{:?}", r.unwrap_err());
        assert!(msg.contains("MAX_WELCOME_BYTES"), "error must cite MAX_WELCOME_BYTES, got: {msg}");
    }
}
