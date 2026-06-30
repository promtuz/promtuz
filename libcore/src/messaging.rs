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
//! 3. Add the recipient via [`MlsGroupHandle::add_members`] →
//!    `(commit, welcome)`.
//! 4. Wrap the welcome via [`make_welcome_envelope`] and publish to
//!    the recipient's K-closest homes via
//!    [`DhtClient::publish_welcome_to_homes`].
//! 5. Merge the founder's own commit; persist the new group_id back
//!    onto the contact row.
//!
//! After lazy-creation (or if the group already exists), application
//! messaging is straightforward:
//!
//! 1. [`MlsGroupHandle::create_application_message`] yields an
//!    `MlsMessageOut`.
//! 2. The bytes are wrapped into [`MlsApplicationEnvelopeP`] with an
//!    outer [`envelope_signing_input`] sig under the sender's IPK
//!    (outer sig binds `to_ipk` so a malicious relay can't redirect).
//! 3. The envelope ships in `DispatchP::payload` over the existing
//!    sticky-home queue.
//!
//! # Receive path
//!
//! `quic/server.rs::handle_deliver` decodes the envelope, dispatches
//! on variant:
//!
//! - **Welcome**: verify outer sig → [`process_welcome`] → persist
//!   group → bind contact's `mls_group_id` → mark consumed KP via
//!   [`KeyPackageStash::on_consumed`].
//! - **Application**: verify outer sig → load group →
//!   [`MlsGroupHandle::process_incoming`] (or buffer in the
//!   epoch-ahead queue if `epoch > current`).
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
//! - [`send_message_inner`] — entry point for outgoing messages
//!   (called by the JNI `sendMessage`).
//! - [`process_inbound_envelope`] — dispatcher for inbound MLS
//!   envelopes; called from `quic/server.rs::handle_deliver`.
//! - [`poll_welcomes`] — drain pending welcomes on reconnect.
//!

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::PROTOCOL_VERSION;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::mls_wire::MAX_FRAMED_MLS_BYTES;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MlsApplicationEnvelopeP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::PairingP;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::envelope_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use log::error;
use log::info;
use log::warn;
use openmls::prelude::BasicCredential;
use openmls::prelude::CredentialWithKey;
use openmls::prelude::KeyPackage;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::ProtocolMessage;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex as PlMutex;
use tokio::sync::Mutex as TokMutex;

use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::identity::IdentitySigner;
use crate::data::message::Message;
use crate::db::mls::stash_db_handle;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PROMTUZ_CIPHERSUITE;
use crate::mls::PromtuzMlsProvider;
use crate::mls::make_welcome_envelope;
use crate::mls::process_welcome;
use crate::mls::types::MlsGroupError;
use crate::quic::dht_client::DhtClient;
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
static GROUP_CREATE_LOCKS: Lazy<PlMutex<HashMap<[u8; 32], Arc<TokMutex<()>>>>> =
    Lazy::new(|| PlMutex::new(HashMap::new()));

/// Acquire (or create) the per-recipient `lazy_create_group` lock.
fn group_create_lock(recipient_ipk: &[u8; 32]) -> Arc<TokMutex<()>> {
    let mut map = GROUP_CREATE_LOCKS.lock();
    map.entry(*recipient_ipk)
        .or_insert_with(|| Arc::new(TokMutex::new(())))
        .clone()
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
fn mint_group_id(creator_ipk: &[u8; 32]) -> [u8; 32] {
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
    let kp_in = KeyPackageIn::tls_deserialize_exact(kp_bytes)
        .map_err(MlsGroupError::from_codec)?;
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
fn build_self_credential(
    own_ipk: &[u8; 32],
) -> Result<
    (openmls_basic_credential::SignatureKeyPair, CredentialWithKey),
    MlsGroupError,
> {
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
/// live relay's peer/1 dialer and hands off to [`send_message_inner`].
/// Errors (rather than silently no-op'ing against a non-wired dialer) if
/// no relay connection is up. Entry point for `api::messaging::send_message`.
pub async fn send(to: [u8; 32], content: String) -> Result<()> {
    // Pull the production peer/1 dialer from the global `RELAY` (attached
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
            send_message_inner(&ctx, to, content).await
        },
        None => {
            error!("MESSAGE: no relay connection / dialer; send aborted");
            Err(anyhow!("not connected to a relay (peer/1 dialer not wired); reconnect first"))
        },
    }
}

/// Eager pairing: create the implicit 1:1 group with `to` and publish a
/// Welcome carrying `pairing` (the inviter's invite + our display name),
/// so a not-yet-contact recipient can gate-accept us. Persists the
/// group_id on success. Entry point for `api::identity::pair_from_qr`.
pub async fn pair(to: [u8; 32], pairing: PairingP) -> Result<()> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    let dht_client = {
        let guard = RELAY.read();
        guard.as_ref().and_then(|r| r.dht_client.clone())
    };
    let client = dht_client
        .ok_or_else(|| anyhow!("not connected to a relay; reconnect before pairing"))?;
    let provider = PromtuzMlsProvider::shared();
    let stash = KeyPackageStash::new(stash_db_handle());
    let buffer = EpochCatchupBuffer::new(stash_db_handle());
    let ctx = MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };

    let group =
        lazy_create_group_paired(&ctx, &our_ipk, &ipk_signer, &to, Some(pairing)).await?;
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
pub async fn lazy_create_group_paired<C: DhtClient>(
    ctx: &MlsContext<'_, C>, our_ipk: &[u8; 32], ipk_signer: &SigningKey, to: &[u8; 32],
    pairing: Option<PairingP>,
) -> Result<MlsGroupHandle> {
    // 1. Fetch peer's KP.
    let fetched = ctx
        .dht
        .fetch_keypackage_for(to)
        .await
        .map_err(|e| anyhow!("fetch_keypackage_for failed: {e}"))?;

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
        let r = kp
            .hash_ref(ctx.provider.crypto())
            .map_err(|e| anyhow!("kp hash_ref: {e:?}"))?;
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
    leaf_kp
        .store(ctx.provider.storage())
        .map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

    // 4. Create group with us as founder.
    let mut group = MlsGroupHandle::create(
        ctx.provider,
        &leaf_kp,
        our_ipk,
        leaf_kp.public(),
        &group_id,
    )
    .map_err(|e| anyhow!("create group: {e}"))?;

    // 5. Add the recipient via their KP.
    let (_commit, welcome) = group
        .add_members(ctx.provider, &leaf_kp, &[kp])
        .map_err(|e| anyhow!("add_members: {e}"))?;

    // 6. Wrap + publish welcome.
    let mut env = make_welcome_envelope(
        welcome, group_id, *our_ipk, *to, kp_ref_used, ipk_signer,
    )
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
    if let Err(e) = ctx.dht.publish_welcome_to_homes(&env).await {
        if let Err(de) = group.delete(ctx.provider) {
            warn!("MLS: publish-failure rollback of group state failed: {de}");
        }
        return Err(anyhow!("publish_welcome_to_homes: {e}"));
    }

    // 7. Merge our own commit.
    group
        .merge_pending_commit(ctx.provider)
        .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

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
pub fn build_application_envelope_bytes<C: DhtClient>(
    ctx: &MlsContext<'_, C>, group: &mut MlsGroupHandle, leaf_signer: &openmls_basic_credential::SignatureKeyPair,
    our_ipk: &[u8; 32], to: &[u8; 32], plaintext: &[u8], ipk_signer: &SigningKey,
) -> Result<Vec<u8>, MlsGroupError> {
    let mls_msg = group.create_application_message(ctx.provider, leaf_signer, plaintext)?;
    let mls_bytes = mls_msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)?;

    if mls_bytes.len() > MAX_FRAMED_MLS_BYTES {
        return Err(MlsGroupError::Internal(format!(
            "mls_bytes {} exceeds MAX_FRAMED_MLS_BYTES = {}",
            mls_bytes.len(),
            MAX_FRAMED_MLS_BYTES
        )));
    }

    let group_id = group.group_id();
    let epoch = group.epoch();

    use ed25519_dalek::Signer;
    let transcript = envelope_signing_input(
        PROTOCOL_VERSION,
        to,
        &group_id,
        epoch,
        &mls_bytes,
    );
    let outer_sig = ipk_signer.sign(&transcript);

    let env = MlsApplicationEnvelopeP {
        version:     MLS_ENVELOPE_VERSION,
        group_id:    group_id.into(),
        epoch,
        mls_message: ByteVec(mls_bytes),
        sender_sig:  outer_sig.to_bytes().into(),
    };
    let outer = MlsEnvelopeP::Application(env);
    let bytes = outer
        .ser()
        .map_err(|e| MlsGroupError::Internal(format!("postcard ser envelope: {e}")))?;

    let _ = our_ipk; // bound only via transcript, not the postcard wire
    Ok(bytes)
}

/// The MLS send path — the replacement for the v2-era shared-key
/// encryption.
///
/// Generic over the [`DhtClient`] backend so unit tests inject a
/// fake; production wires the real dialer.
pub async fn send_message_inner<C: DhtClient>(
    ctx: &MlsContext<'_, C>, to: [u8; 32], content: String,
) -> Result<()> {
    // 0. Save to local DB first (status = pending).
    let msg = Message::save_outgoing(to, &content)?;
    let msg_id = msg.inner.id;
    let msg_timestamp = msg.inner.timestamp;

    // 1. Look up the contact.
    let contact = match Contact::get(&to) {
        Some(c) => c,
        None => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "recipient not in contacts".into() }.emit();
            return Err(anyhow!("recipient not in contacts"));
        },
    };

    // 2. Identity material we need for both sign + group lazy-create.
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();

    // The MLS layer needs the IPK *secret half* for envelope signing.
    // We never let it leak — `IdentitySigner::sign` decrypts and signs
    // in one shot, but for the multi-step send path we hold a
    // `SigningKey` for the duration of the call (zeroized on drop).
    let ipk_signer: SigningKey = {
        // Keystore decrypt happens behind IdentitySigner; for the MLS
        // path we open the data layer one extra time. The Zeroizing
        // wrapper is dropped at the end of this block.
        IdentitySigner::sign(b"phase4 ipk lookup").map_err(|e| anyhow!("ipk lookup: {e}"))?;
        // The `data::identity::Identity::secret_key_with_manager` is
        // module-private. We expose the secret via the small helper
        // below.
        crate::data::identity::secret_key_signing(&our_ipk)?
    };

    // 3. Resolve or lazy-create the implicit 1:1 group.
    //
    // Hold a per-recipient `tokio::sync::Mutex` for the entire "check
    // existing group → fetch KP → build group →
    // publish Welcome → persist group_id" critical section so two
    // concurrent first-sends to the same contact do not both
    // lazy-create. The acquire is cheap when uncontested; the lock
    // never crosses an `await` outside the critical section.
    let mut group = {
        let lock = group_create_lock(&to);
        let _guard = lock.lock().await;
        // Re-read the contact after acquiring the lock — the racing
        // task may have just persisted a fresh `mls_group_id`.
        let contact_now = Contact::get(&to).unwrap_or(contact);
        match contact_now.inner.mls_group_id {
            Some(gid) => match MlsGroupHandle::load(ctx.provider, &gid) {
                Ok(Some(g)) => g,
                Ok(None) => {
                    // The contact references a group_id we no longer have
                    // local state for — the libcore DB and SQLite drifted.
                    // Fall back to lazy-create (and overwrite the stale id).
                    warn!(
                        "MESSAGE: contact's mls_group_id has no local state; recreating"
                    );
                    let g = lazy_create_group(ctx, &our_ipk, &ipk_signer, &to).await?;
                    if let Err(e) = Contact::set_mls_group_id(&to, &g.group_id()) {
                        warn!("MESSAGE: persist mls_group_id failed: {e}");
                    }
                    g
                },
                Err(e) => {
                    Message::mark_failed(&msg_id);
                    MessageEv::Failed { id: msg_id, to, reason: format!("load group: {e}") }
                        .emit();
                    return Err(anyhow!("load group: {e}"));
                },
            },
            None => match lazy_create_group(ctx, &our_ipk, &ipk_signer, &to).await {
                Ok(g) => {
                    if let Err(e) = Contact::set_mls_group_id(&to, &g.group_id()) {
                        warn!("MESSAGE: persist mls_group_id failed: {e}");
                    }
                    g
                },
                Err(e) => {
                    Message::mark_failed(&msg_id);
                    MessageEv::Failed { id: msg_id, to, reason: e.to_string() }.emit();
                    return Err(e);
                },
            },
        }
    };

    // 4. The leaf signing key for this member's seat. After
    //    lazy-create the founder's signer is stored in the openmls
    //    storage by the credential lookup; we re-derive a transient
    //    one for application messages by reading it back.
    let leaf_kp = leaf_signer_for_group(ctx.provider, &group, &our_ipk)?;

    // 5. Build the application envelope.
    let payload = build_application_envelope_bytes(
        ctx,
        &mut group,
        &leaf_kp,
        &our_ipk,
        &to,
        content.as_bytes(),
        &ipk_signer,
    )
    .map_err(|e| {
        Message::mark_failed(&msg_id);
        anyhow!("build envelope: {e}")
    })?;

    // 6. Outer DispatchP, signed under the sender's IPK.
    let id: [u8; 16] = uuid::Uuid::now_v7().into_bytes();
    let sig_message = dispatch_sig_message(&to, &our_ipk, &id, &payload);
    let sig = IdentitySigner::sign(&sig_message)?.to_bytes();
    let fwd = DispatchP {
        to:      Bytes(to),
        from:    Bytes(our_ipk),
        id:      Bytes(id),
        payload: ByteVec(payload),
        sig:     Bytes(sig),
    };

    // 7. Send via the existing relay channel.
    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone()).ok_or_else(|| {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "not connected to relay".into() }.emit();
            anyhow!("not connected to relay")
        })?
    };

    let (mut send, mut recv) = conn.open_bi().await?;
    CRelayPacket::Dispatch(fwd).send(&mut send).await?;
    send.finish()?;

    // 8. Wait for the relay's ack.
    match SRelayPacket::unpack(&mut recv).await? {
        SRelayPacket::DispatchAck(DispatchAckP::Queued)
        | SRelayPacket::DispatchAck(DispatchAckP::Forwarded)
        | SRelayPacket::DispatchAck(DispatchAckP::Delivered) => {
            info!("MESSAGE: sent to {}", hex::encode(&to[..4]));
            Message::mark_sent(&msg_id);
            MessageEv::Sent { id: msg_id, to, content, timestamp: msg_timestamp }.emit();
        },
        SRelayPacket::DispatchAck(DispatchAckP::NotFound) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "recipient not found".into() }.emit();
        },
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "invalid signature".into() }.emit();
        },
        SRelayPacket::DispatchAck(DispatchAckP::Error { reason }) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason }.emit();
        },
        SRelayPacket::DispatchAck(DispatchAckP::QueueFull) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "queue full".into() }.emit();
        },
        other => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: format!("unexpected: {other:?}") }.emit();
        },
    }

    Ok(())
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
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], payload: &[u8],
) -> Result<Option<InboundDecoded>> {
    let envelope = MlsEnvelopeP::deser(payload)
        .map_err(|e| anyhow!("postcard deser MlsEnvelopeP: {e}"))?;

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
    }

    match envelope {
        MlsEnvelopeP::Welcome(env) => process_welcome_inbound(ctx, sender_ipk, env)
            .map(|_| Some(InboundDecoded::Welcome)),
        MlsEnvelopeP::Application(env) => {
            process_application_inbound(ctx, sender_ipk, env).map(Some)
        },
    }
}

/// Result of [`process_inbound_envelope`] — what kind of payload was
/// processed (UI / metrics consumer).
#[allow(dead_code)] // ApplicationStale is reachable only when the
// receive path passes a stale message through. The current dispatcher
// folds it into ApplicationBuffered; the explicit stale-drop path is
// future work.
#[derive(Debug)]
pub enum InboundDecoded {
    /// Welcome processed; group activated. The caller probably wants
    /// to emit an "added to group" UI event (future work).
    Welcome,
    /// Application message decrypted. `group_id` is surfaced for
    /// future UI threading work.
    Application {
        plaintext: Vec<u8>,
        #[allow(dead_code)]
        group_id:  [u8; 32],
    },
    /// Application message buffered for a future epoch.
    ApplicationBuffered,
    /// Application message stale (epoch < current); dropped.
    ApplicationStale,
}

fn process_welcome_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: WelcomeEnvelopeP,
) -> Result<()> {
    if env.sender_ipk.0 != sender_ipk {
        bail!("welcome envelope sender_ipk mismatch with DispatchP.from");
    }

    // Defensive check that the envelope is addressed to *us*. The
    // outer `sender_sig` transcript already binds `recipient_ipk`,
    // so a malicious sender cannot address-spoof another recipient.
    // But a delivery-layer bug could deliver an envelope intended for
    // a different device to this one; surface that as a typed error
    // instead of silently activating a group we don't belong to.
    let our_ipk = Identity::get()
        .ok_or_else(|| anyhow!("identity not found"))?
        .ipk();
    if env.recipient_ipk.0 != our_ipk {
        warn!(
            "MLS: dropped Welcome addressed to {} (we are {})",
            hex::encode(&env.recipient_ipk.0[..4]),
            hex::encode(&our_ipk[..4])
        );
        bail!("welcome envelope recipient_ipk does not match self");
    }

    // Contact-or-invite gate: accept a Welcome from a stranger only if it
    // carries a valid pairing invite we minted (verified under our own IPK,
    // unexpired). On first pair we save them as a contact using the
    // self-asserted display name (length-bounded). Welcomes from existing
    // contacts skip the invite check.
    if !Contact::exists(&sender_ipk) {
        let invited =
            env.pairing.as_ref().is_some_and(|p| Identity::verify_invite(&p.invite));
        if !invited {
            warn!(
                "MLS: dropped Welcome from unknown sender {} (no valid invite)",
                hex::encode(&sender_ipk[..4])
            );
            bail!("unknown sender and no valid invite");
        }
        if let Some(p) = env.pairing.as_ref() {
            let name: String = p.sender_name.chars().take(32).collect();
            match Contact::save(sender_ipk, name) {
                Ok(_) => info!("IDENTITY: paired with {}", hex::encode(&sender_ipk[..4])),
                Err(e) => warn!("IDENTITY: failed to save paired contact: {e}"),
            }
        }
    }

    let group = process_welcome_inbound_no_contacts(ctx, sender_ipk, env)?;

    // Persist the group_id back onto the contact row so the next send
    // to this contact reuses it. We tolerate "no contact row" silently
    // — the contact-first gate above means this branch is unreachable
    // for legitimate Welcomes; defence-in-depth only.
    let _ = Contact::set_mls_group_id(&sender_ipk, &group.group_id());

    info!(
        "MLS: welcome from {} activated group {}",
        hex::encode(&sender_ipk[..4]),
        hex::encode(&group.group_id()[..4])
    );
    Ok(())
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
    let group = process_welcome(ctx.provider, &env)
        .map_err(|e| anyhow!("process_welcome: {e}"))?;
    if let Err(e) = ctx.stash.on_consumed(&kp_ref) {
        warn!("MLS: stash on_consumed failed: {e}");
    }
    Ok(group)
}

fn process_application_inbound<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], env: MlsApplicationEnvelopeP,
) -> Result<InboundDecoded> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    process_application_inbound_for(ctx, sender_ipk, &our_ipk, env)
}

/// Explicit-recipient variant of [`process_application_inbound`].
/// Identical pipeline but takes `our_ipk` as an argument instead of
/// reading the global `Identity::get()`. Production
/// `process_application_inbound` delegates here. The e2e harness uses
/// this directly so each test client can assert against its own IPK
/// without sharing a process-global identity row.
pub fn process_application_inbound_for<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], our_ipk: &[u8; 32],
    env: MlsApplicationEnvelopeP,
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
    vk.verify_strict(&transcript, &sig)
        .map_err(|_| anyhow!("application envelope sig invalid"))?;

    // 2. Look up the group.
    let mut group = MlsGroupHandle::load(ctx.provider, &env.group_id.0)
        .map_err(|e| anyhow!("load group: {e}"))?
        .ok_or_else(|| anyhow!("no local state for group"))?;

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
            .push(&group, env.mls_message.0.clone(), env.epoch, dispatch_id)
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
    let proto: ProtocolMessage = in_msg
        .try_into_protocol_message()
        .map_err(|e| anyhow!("not a ProtocolMessage: {e:?}"))?;

    let processed = group
        .process_incoming(ctx.provider, proto)
        .map_err(|e| anyhow!("process_incoming: {e}"))?;

    match processed {
        ProcessedMessageContent::ApplicationMessage(app) => {
            let plaintext = app.into_bytes();
            // After every successful processing, drain any buffered
            // ahead-of-epoch messages.
            let _ = ctx.buffer.drain_when_ready(&mut group, ctx.provider);
            Ok(InboundDecoded::Application {
                plaintext,
                group_id: env.group_id.0,
            })
        },
        ProcessedMessageContent::StagedCommitMessage(staged) => {
            group
                .merge_staged_commit(ctx.provider, *staged)
                .map_err(|e| anyhow!("merge_staged_commit: {e}"))?;
            // After commit-merge, drain any newly-processable buffered
            // messages.
            let _ = ctx.buffer.drain_when_ready(&mut group, ctx.provider);
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
///   - **Known sender, bad processing**: ack (a known contact sending
///     us malformed welcomes is a contact-flow bug, not abuse).
///   - **Unknown sender or bad sig**: hold without acking; bump a
///     per-`welcome_id` retry counter. Ack-and-drop only after
///     `POLL_WELCOMES_MAX_RETRY` failed attempts. This preserves the
///     evidence the future anti-abuse spec (HANDOFF priority 2) wants
///     while still bounding queue growth.
#[allow(dead_code)] // The production call site is wired in quic/server.
pub async fn poll_welcomes<C: DhtClient>(ctx: &MlsContext<'_, C>) -> Result<usize> {
    let entries = ctx
        .dht
        .fetch_welcomes()
        .await
        .map_err(|e| anyhow!("fetch_welcomes: {e}"))?;

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
            Ok(()) => {
                ack_ids.push(welcome_id);
                count += 1;
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
            MlsContext {
                provider: &self.provider,
                stash:    &self.stash,
                buffer:   &self.buffer,
                dht,
            }
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .expect("seed bob's published kps");

        // Alice runs lazy_create_group.
        let g = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
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
        let bob_group = process_welcome(&bob.provider, &entry.envelope)
            .expect("process welcome");
        assert_eq!(bob_group.epoch(), g.epoch());
        assert_eq!(bob_group.member_count(), g.member_count());
        assert_eq!(bob_group.group_id(), g.group_id());
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .unwrap();

        // Alice creates the group + Welcome; Bob materialises.
        let mut alice_group = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
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
        let mut bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();
        use openmls::prelude::tls_codec::Deserialize as _;
        let in_msg = openmls::prelude::MlsMessageIn::tls_deserialize_exact(
            &env_app.mls_message.0,
        )
        .unwrap();
        let proto: ProtocolMessage = in_msg.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, proto).unwrap();
        match processed {
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .unwrap();

        let mut alice_group = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
        .await
        .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");
        let mut bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();

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
            .push(&bob_group, mls_bytes, alice_group.epoch(), dispatch_id.clone())
            .expect("push");
        assert_eq!(outcome, crate::mls::PushOutcome::Inserted);

        let drained = bob
            .buffer
            .drain_when_ready(&mut bob_group, &bob.provider)
            .expect("drain");
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .unwrap();

        let g = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .unwrap();

        let mut alice_group = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
        .await
        .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");

        // Force bob_group to advance one epoch via Update so its epoch
        // is strictly greater than what alice will sign in the test.
        // Easier: build an envelope claiming epoch 0 and feed it to
        // a freshly-loaded bob_group whose epoch is 1.
        let bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();
        assert!(bob_group.epoch() >= 1, "fresh group is at epoch >= 1");

        // Build an envelope at epoch 0 (stale by design).
        let alice_leaf =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
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
        let result = process_application_inbound_for(
            &bob.ctx(dht.as_ref()),
            alice.ipk,
            &bob.ipk,
            env,
        )
        .expect("stale envelope returns ApplicationStale, not Err");

        match result {
            InboundDecoded::ApplicationStale => {},
            other => panic!("expected ApplicationStale, got {other:?}"),
        }

        let buffered_after = bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        assert_eq!(
            buffered_before, buffered_after,
            "stale envelope must not grow the buffer"
        );
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
        dht.publish_keypackages(
            &bob_kps[..1],
            crate::quic::dht_client::KpOutcomeFilter::Default,
        )
        .await
        .unwrap();

        let mut alice_group = lazy_create_group(
            &alice.ctx(dht.as_ref()),
            &alice.ipk,
            &alice.ipk_signer,
            &bob.ipk,
        )
        .await
        .unwrap();
        let entries = dht.fetch_welcomes().await.unwrap();
        let _ = process_welcome(&bob.provider, &entries[0].envelope).expect("process welcome");

        let alice_leaf =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let mls_msg = alice_group
            .create_application_message(&alice.provider, &alice_leaf, b"far future")
            .unwrap();
        let mls_bytes =
            openmls::prelude::tls_codec::Serialize::tls_serialize_detached(&mls_msg).unwrap();

        let bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();
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

        let buffered_before =
            bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        let result = process_application_inbound_for(
            &bob.ctx(dht.as_ref()),
            alice.ipk,
            &bob.ipk,
            env,
        );
        assert!(result.is_err(), "far-future envelope must be rejected");
        let buffered_after =
            bob.buffer.buffered_count(&alice_group.group_id()).unwrap_or(0);
        assert_eq!(
            buffered_before, buffered_after,
            "far-future envelope must not grow the buffer"
        );
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

        let r = process_inbound_envelope(&bob.ctx(dht.as_ref()), [0u8; 32], &bytes).await;
        assert!(r.is_err(), "oversize welcome must be rejected at decode");
        let msg = format!("{:?}", r.unwrap_err());
        assert!(
            msg.contains("MAX_WELCOME_BYTES"),
            "error must cite MAX_WELCOME_BYTES, got: {msg}"
        );
    }
}
