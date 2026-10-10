//! MLS sessions: creating, finding and repairing the group a chat sends through.

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::PairingP;
use common::utils::now_ms;
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use log::info;
use log::warn;
use openmls::prelude::CredentialWithKey;
use openmls::prelude::KeyPackage;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use tokio::sync::Mutex as TokMutex;

use super::welcome::valid_initial_pair;
use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PROMTUZ_CIPHERSUITE;
use crate::mls::PromtuzMlsProvider;
use crate::mls::make_welcome_envelope;
use crate::mls::types::MlsGroupError;
use crate::quic::dht_client::DhtClient;
use crate::state::core;

/// `scope` is a conversation id on the send path and a peer IPK on the inbound heal path.
fn group_create_lock(scope: &[u8]) -> Arc<TokMutex<()>> {
    let mut map = core().messaging.group_create.lock();
    map.entry(scope.to_vec()).or_insert_with(|| Arc::new(TokMutex::new(()))).clone()
}

/// Unique by construction: the random suffix is private to the creator.
pub(crate) fn mint_group_id(creator_ipk: &[u8; 32]) -> [u8; 32] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;

    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    let now_ms = now_ms();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"promtuz-mls-v1 group-id");
    hasher.update(creator_ipk);
    hasher.update(&now_ms.to_be_bytes());
    hasher.update(&nonce);
    *hasher.finalize().as_bytes()
}

/// Rejects any cipher suite other than [`PROMTUZ_CIPHERSUITE`].
fn decode_keypackage_bytes(kp_bytes: &[u8]) -> Result<KeyPackage, MlsGroupError> {
    use openmls::prelude::KeyPackageIn;
    use openmls::prelude::ProtocolVersion;
    let kp_in = KeyPackageIn::tls_deserialize_exact(kp_bytes).map_err(MlsGroupError::from_codec)?;
    let kp = kp_in
        .validate(
            &openmls_rust_crypto::RustCrypto::default(),
            ProtocolVersion::Mls10,
        )
        .map_err(|e| MlsGroupError::Internal(format!("KeyPackageIn::validate: {e:?}")))?;
    if kp.ciphersuite() != PROMTUZ_CIPHERSUITE {
        return Err(MlsGroupError::BadCipherSuite);
    }
    Ok(kp)
}

/// A fresh leaf signing key, distinct from the IPK so a leaf compromise cannot recover it, bound
/// to the identity by the IPK's signature. The caller stores the key pair before creating a group.
pub(crate) fn build_self_credential(
    ipk_signer: &SigningKey,
) -> Result<(openmls_basic_credential::SignatureKeyPair, CredentialWithKey), MlsGroupError> {
    let leaf_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
        .map_err(|e| MlsGroupError::Internal(format!("leaf signature key: {e:?}")))?;
    let credential = crate::mls::credential::bound_credential(ipk_signer, leaf_kp.public());
    let cwk = CredentialWithKey {
        credential:    credential.into(),
        signature_key: leaf_kp.public().into(),
    };
    Ok((leaf_kp, cwk))
}

pub struct MlsContext<'a, C: DhtClient> {
    pub provider: &'a PromtuzMlsProvider,
    pub stash:    &'a KeyPackageStash,
    pub buffer:   &'a EpochCatchupBuffer,
    pub dht:      &'a C,
}

pub async fn lazy_create_group<C: DhtClient>(
    ctx: &MlsContext<'_, C>, our_ipk: &[u8; 32], ipk_signer: &SigningKey, to: &[u8; 32],
) -> Result<MlsGroupHandle> {
    lazy_create_group_paired(ctx, our_ipk, ipk_signer, to, None).await
}

/// Returns the KeyPackage with the `kp_ref` naming the consumed record. The `owner_sig` re-check
/// is defence in depth against a replica forwarding a tampered record.
pub(crate) async fn fetch_verified_keypackage<C: DhtClient>(
    ctx: &MlsContext<'_, C>, who: &[u8; 32], strict: bool,
) -> Result<(KeyPackage, [u8; 32])> {
    // Keep `DhtClientError` downcastable, never stringified: callers detect a `NoStash` miss to
    // defer the send.
    let record = ctx
        .dht
        .fetch_keypackage_for(who)
        .await
        .map_err(|e| anyhow::Error::new(e).context("fetch_keypackage_for"))?;
    {
        use common::proto::mls_wire::MLS_WIRE_VERSION;
        use common::proto::mls_wire::kp_record_signing_input;
        let vk = VerifyingKey::from_bytes(&record.ipk.0)
            .map_err(|e| anyhow!("recipient ipk is not valid Ed25519: {e}"))?;
        if &record.ipk.0 != who {
            bail!("fetched KP's owner ipk does not match the member requested");
        }
        let sig = Signature::from_bytes(&record.owner_sig.0);
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &record.ipk.0,
            &record.kp_ref.0,
            &record.kp_bytes.0,
            record.expires_at_ms,
        );
        vk.verify_strict(&msg, &sig).map_err(|e| anyhow!("owner_sig invalid: {e}"))?;
    }
    let kp = decode_keypackage_bytes(&record.kp_bytes.0)
        .map_err(|e| anyhow!("decode KP: {e}"))?;
    // The leaf must be bound to `who`. `strict` refuses a legacy leaf: fine for a pair, which the
    // owner_sig ties to `who`, but group members read the roster off the leaves alone.
    if crate::mls::credential::leaf_node_ipk(kp.leaf_node(), strict) != Some(*who) {
        bail!("fetched KP's leaf is not bound to the member requested");
    }
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

/// Attaches `pairing` to the Welcome so a recipient who is not yet a contact can accept it.
pub async fn lazy_create_group_paired<C: DhtClient>(
    ctx: &MlsContext<'_, C>, our_ipk: &[u8; 32], ipk_signer: &SigningKey, to: &[u8; 32],
    pairing: Option<PairingP>,
) -> Result<MlsGroupHandle> {
    // A legacy leaf will do: a pair has nobody in it to impersonate.
    let (kp, kp_ref_used) = fetch_verified_keypackage(ctx, to, false).await?;

    let group_id = mint_group_id(our_ipk);

    let (leaf_kp, cwk) =
        build_self_credential(ipk_signer).map_err(|e| anyhow!("build credential: {e}"))?;
    leaf_kp.store(ctx.provider.storage()).map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

    let mut group =
        // No meta: a pairing group is a 1:1, and that absence is exactly how
        // the far side knows not to open a group chat for it.
        MlsGroupHandle::create(ctx.provider, &leaf_kp, cwk, &group_id, None)
            .map_err(|e| anyhow!("create group: {e}"))?;

    let (_commit, welcome) = group
        .add_members(ctx.provider, &leaf_kp, &[kp])
        .map_err(|e| anyhow!("add_members: {e}"))?;

    let mut env = make_welcome_envelope(welcome, group_id, *our_ipk, *to, kp_ref_used, ipk_signer)
        .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
    env.pairing = pairing;

    if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
        // Roll back the half-built group so its state does not linger in storage.
        if let Err(de) = group.delete(ctx.provider) {
            warn!("MLS: oversize-welcome rollback failed: {de}");
        }
        bail!(
            "welcome blob {} exceeds MAX_WELCOME_BYTES = {}",
            env.welcome_blob.0.len(),
            MAX_WELCOME_BYTES
        );
    }

    // On failure roll back the group, or we would keep a group the peer never got a Welcome for
    // and every later send would encrypt to a group of one.
    if let Err(e) = ctx.dht.deliver_welcome(&env).await {
        if let Err(de) = group.delete(ctx.provider) {
            warn!("MLS: welcome-delivery rollback of group state failed: {de}");
        }
        return Err(anyhow!("deliver_welcome: {e}"));
    }

    group.merge_pending_commit(ctx.provider).map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

    Ok(group)
}

/// Lazy-creates the group on a direct chat's first send, under a per-conversation lock so two
/// concurrent first sends cannot both create one. Group chats are never lazy-created.
pub(super) async fn group_for_conversation<C: DhtClient>(
    ctx: &MlsContext<'_, C>, conversation: &[u8; 16], our_ipk: &[u8; 32], ipk_signer: &SigningKey,
) -> Result<MlsGroupHandle> {
    ensure!(!crate::requests::is_request_chat(conversation), "Accept the request first");
    let lock = group_create_lock(conversation);
    let _guard = lock.lock().await;

    // Re-read under the lock: a racing send may have just bound a group.
    let row = Conversation::get(conversation)
        .ok_or_else(|| anyhow!("no such conversation {}", hex::encode(&conversation[..4])))?;
    let bound: Option<[u8; 32]> =
        row.mls_group_id.as_ref().and_then(|g| g.as_slice().try_into().ok());

    if let Some(gid) = bound {
        match MlsGroupHandle::load(ctx.provider, &gid) {
            Ok(Some(g)) => {
                // A damaged pair session is replaceable without removing its
                // conversation. A real group must use its authenticated recovery.
                if row.kind != crate::data::conversation::KIND_DIRECT
                    || (Conversation::peer_of(conversation).is_some_and(|peer|
                        valid_initial_pair(&g.roster(), g.is_group_chat(), our_ipk, &peer))
                        && leaf_signer_for_group(ctx.provider, &g, our_ipk).is_ok()) {
                    return Ok(g);
                }
                warn!("MESSAGE: repairing unusable direct-chat encryption");
            },
            // The conversation points at a group with no local state. Recreate and repoint it;
            // history is keyed on the conversation, so it stays.
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
    let status = Contact::status(&peer);
    if status.is_some_and(|s| s != crate::data::contact::PAIR_STATUS_PENDING) {
        // Keep the address book's shortcut in step so pairing-era lookups agree.
        if let Err(e) = Contact::set_mls_group_id(&peer, &group.group_id()) {
            warn!("MESSAGE: persist mls_group_id failed: {e}");
        }
        return Ok(group);
    }
    // Our first message to someone who never added us, or who deleted our pair since, lands as a
    // request. Profile access follows acceptance, independently of this MLS group.
    if status.is_none() {
        Contact::save_pending(peer, String::new())?;
    }
    Contact::set_mls_group_id(&peer, &group.group_id())?;
    Ok(group)
}

pub fn leaf_signer_for_group(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, our_ipk: &[u8; 32],
) -> Result<openmls_basic_credential::SignatureKeyPair> {
    ensure!(
        !group.is_group_chat() || group.group_meta().is_some(),
        "this group's rules are not supported by this app version"
    );
    let leaf_idx = group
        .member_index_by_ipk(our_ipk)
        .ok_or_else(|| anyhow!("our IPK is not a member of group"))?;
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

/// A known contact wrote into a pair group we hold no state for: mint a fresh one so the next
/// messages flow. The lost ciphertext stays lost.
pub(super) async fn heal_dead_group<C: DhtClient>(
    ctx: &MlsContext<'_, C>, sender_ipk: [u8; 32], dead_gid: &[u8; 32],
) {
    if !Contact::exists(&sender_ipk) {
        return;
    }
    let Some(our_ipk) = Identity::local_ipk() else { return };
    let Ok(ipk_signer) = crate::data::identity::secret_key_signing(&our_ipk) else { return };

    let Some(conversation) = Conversation::for_group(dead_gid) else { return };
    if Conversation::peer_of(&conversation) != Some(sender_ipk) { return; }
    let lock = group_create_lock(&conversation);
    let _guard = lock.lock().await;

    // Heal only while the dead id is still the pair group on the contact row; the first heal of a
    // backlog repoints it. A row with no pair group is ambiguous, so our own first send heals it.
    let current = Contact::get(&sender_ipk).and_then(|c| c.inner.mls_group_id);
    if current != Some(*dead_gid) || Conversation::group_of(&conversation) != Some(*dead_gid) {
        return;
    }
    match lazy_create_group(ctx, &our_ipk, &ipk_signer, &sender_ipk).await {
        Ok(g) => {
            if let Err(e) = Conversation::bind_group(&conversation, &g.group_id()) {
                warn!("MLS: could not bind repaired direct chat: {e}");
                return;
            }
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

#[cfg(test)]
mod tests {
    use openmls::prelude::MlsMessageIn;

    use super::*;
    use crate::data::message::Message;
    use crate::messaging::send::seal_application_message;
    use crate::mls::process_welcome;
    use crate::test_support::ScopedCore;
    use crate::test_support::data::identity;
    use crate::test_support::net::Device;
    use crate::test_support::net::FakeDhtClient;

    /// Partial key loss and two racing repairs converge on one new session that keeps the chat,
    /// its history and the contact binding, and the repaired pair decrypts.
    #[tokio::test]
    async fn damaged_direct_session_repairs_once_without_replacing_the_chat() {
        let scope = ScopedCore::new();
        let (alice, bob, dht) = (Device::new(81), Device::new(82), FakeDhtClient::default());
        identity(&scope.core.db.identity().lock(), 81);
        Contact::save(bob.ipk, "Bob".into()).unwrap();
        let conversation = Conversation::for_peer(&bob.ipk).unwrap();
        bob.publish_keypackage(&dht).await;
        bob.publish_keypackage(&dht).await;
        let ctx = alice.ctx(&dht);
        let repair = || group_for_conversation(&ctx, &conversation, &alice.ipk, &alice.signer);
        let original = repair().await.unwrap();
        let history = Message::save_outgoing(conversation, "Retain this history", None).unwrap();
        // The tree survives; its signing key does not.
        let lost = "DELETE FROM mls_storage WHERE group_id = X'' AND key_tag = ?1";
        let tag = crate::mls::storage::tags::SIGNATURE_KEY_PAIR;
        alice.db.mls().lock().execute(lost, [tag]).unwrap();
        assert!(leaf_signer_for_group(&alice.provider, &original, &alice.ipk).is_err());

        let (first, second) = tokio::join!(repair(), repair());
        let mut fresh = first.unwrap();
        let gid = fresh.group_id();
        assert_ne!(gid, original.group_id());
        assert_eq!(second.unwrap().group_id(), gid, "racing repairs share one session");
        assert_eq!(Conversation::for_peer(&bob.ipk).unwrap(), conversation);
        assert_eq!(Conversation::group_of(&conversation), Some(gid));
        assert_eq!(Contact::get(&bob.ipk).unwrap().inner.mls_group_id, Some(gid));
        let did = history.inner.dispatch_id.try_into().unwrap();
        let kept = Message::get_by_dispatch(&conversation, &did).unwrap();
        assert_eq!(kept.inner.content, "Retain this history");

        let welcomes = dht.welcomes_pending.lock().clone();
        let welcome = welcomes.iter().find(|w| w.envelope.group_id.0 == gid).unwrap();
        let mut joined = process_welcome(&bob.provider, &welcome.envelope).unwrap();
        let leaf = leaf_signer_for_group(&alice.provider, &fresh, &alice.ipk).unwrap();
        let sealed = seal_application_message(&alice.provider, &mut fresh, &leaf, b"hi").unwrap();
        let message = MlsMessageIn::tls_deserialize_exact(&sealed.mls_bytes)
            .unwrap()
            .try_into_protocol_message()
            .unwrap();
        assert_eq!(joined.process_incoming(&bob.provider, message).unwrap().sender, alice.ipk);
    }
}
