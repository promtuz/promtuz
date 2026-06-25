//! Welcome envelope construction and processing (spec §3.3, §4.2,
//! §6.6, §12.7).
//!
//! # Outbound (`make_welcome_envelope`)
//!
//! Called by an inviter after `MlsGroupHandle::add_members` returns
//! its `(commit, welcome)` pair. We:
//! 1. TLS-serialise the openmls `Welcome` (carried inside the
//!    returned `MlsMessageOut`).
//! 2. Compute the outer signing transcript via
//!    `welcome_envelope_signing_input` (spec §3.3).
//! 3. Sign with the inviter's IPK long-term key.
//! 4. Pack into [`WelcomeEnvelopeP`].
//!
//! The outer envelope sig binds `(group_id, sender_ipk,
//! recipient_ipk, kp_ref_used, BLAKE3(welcome_blob))` so a captured
//! Welcome cannot be re-targeted at a different recipient or
//! re-paired with a different KeyPackage.
//!
//! # Inbound (`process_welcome`)
//!
//! 1. Verify outer sig under `sender_ipk`. Reject on failure (spec
//!    §12.7).
//! 2. TLS-deserialise the inner `MlsMessageIn`, ensure it carries a
//!    `Welcome` body.
//! 3. Hand to `StagedWelcome::new_from_welcome` — openmls looks up
//!    the matching KeyPackageBundle via its storage provider
//!    (`mls_storage.key_tag = KEY_PACKAGE`); on success, decrypts
//!    and gives us a `StagedWelcome`.
//! 4. Promote into a real group via `into_group`.
//! 5. Wrap in [`MlsGroupHandle`].
//!
//! # KeyPackage consumption marking
//!
//! Per spec §5.3, openmls itself deletes the consumed KP from its
//! storage during `into_group` (the default is *not* a "last-resort"
//! KP, so the storage trait's `delete_key_package` is invoked).
//! That handles the openmls-internal side. For our stash tracking
//! (which lives in a separate table), the caller invokes
//! `KeyPackageStash::on_consumed` after `process_welcome` returns.
//!
//! design-doc: `misc/specs/MLS.md` §3.3 (signing transcript), §4.2
//! step 8 (recipient flow), §12.7 (Welcome injection defence).

// Public surface here (`make_welcome_envelope` / `process_welcome`)
// is consumed by `messaging.rs`; the cdylib compiler can't see across
// the JNI boundary so flags it as dead. Mirrors the `provider.rs`
// pattern.
#![allow(dead_code)]

use common::proto::mls_wire::welcome_envelope_signing_input;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer as DalekSigner;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::MlsGroupJoinConfig;
use openmls::prelude::MlsMessageBodyIn;
use openmls::prelude::MlsMessageIn;
use openmls::prelude::MlsMessageOut;
use openmls::prelude::StagedWelcome;

use super::group::MlsGroupHandle;
use super::provider::PromtuzMlsProvider;
use super::types::MlsGroupError;

type Result<T> = std::result::Result<T, MlsGroupError>;

/// Build a [`WelcomeEnvelopeP`] from an openmls `MlsMessageOut`
/// produced by [`MlsGroupHandle::add_members`].
///
/// `welcome_msg` must carry an `Welcome` body (the second member of
/// the `(commit, welcome)` tuple from `add_members`); we surface a
/// `MlsGroupError::Internal` if it doesn't, matching the spec
/// invariant that Add commits always emit a Welcome.
///
/// `signer` is the inviter's IPK long-term `SigningKey`. We ask for
/// the concrete dalek type rather than a trait because the IPK is
/// already stored that way in promtuz (`Identity::secret_key_with_manager`
/// returns a `Zeroizing<SecretKey>` which the caller upgrades to
/// `SigningKey`); abstracting via a trait would force callers to
/// thread an extra bound across the FFI boundary.
///
/// `kp_ref_used` is the SHA-256 KeyPackageRef of the recipient's
/// KP this Welcome consumes — opaque to promtuz; the recipient
/// echoes it for diagnostic / dedup purposes.
pub fn make_welcome_envelope(
    welcome_msg: MlsMessageOut, group_id: [u8; 32], sender_ipk: [u8; 32],
    recipient_ipk: [u8; 32], kp_ref_used: [u8; 32], signer: &SigningKey,
) -> Result<WelcomeEnvelopeP> {
    // **Implementation note (openmls 0.8 deviation from spec)**:
    // `MlsMessageOut::into_welcome()` is gated by
    // `#[cfg(any(test, feature = "test-utils"))]` in openmls 0.8 —
    // we cannot extract the inner `Welcome` value in production
    // code. Instead we TLS-serialise the **entire `MlsMessageOut`**
    // (which carries `wire_format = welcome` plus the body). The
    // recipient TLS-deserialises as `MlsMessageIn` and pattern-
    // matches on `MlsMessageBodyIn::Welcome`. Functionally
    // equivalent to "TLS-encoded Welcome" per spec §3.1; the only
    // difference is the outer 1-byte `version` field that prefixes
    // the message body. Documented in the [`process_welcome`] doc
    // comment.
    let welcome_blob = welcome_msg
        .tls_serialize_detached()
        .map_err(MlsGroupError::from_codec)?;

    if welcome_blob.len() > MAX_WELCOME_BYTES {
        return Err(MlsGroupError::Internal(format!(
            "welcome_blob {} bytes exceeds MAX_WELCOME_BYTES = {}",
            welcome_blob.len(),
            MAX_WELCOME_BYTES
        )));
    }

    // 2. Compute the signing transcript per spec §3.3.
    let transcript = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &group_id,
        &sender_ipk,
        &recipient_ipk,
        &kp_ref_used,
        &welcome_blob,
    );

    // 3. Sign under the IPK.
    let sig = signer.sign(&transcript);

    Ok(WelcomeEnvelopeP {
        version: MLS_ENVELOPE_VERSION,
        group_id: group_id.into(),
        sender_ipk: sender_ipk.into(),
        recipient_ipk: recipient_ipk.into(),
        welcome_blob: welcome_blob.into(),
        kp_ref_used: kp_ref_used.into(),
        sender_sig: sig.to_bytes().into(),
    })
}

/// Verify and process an inbound [`WelcomeEnvelopeP`].
///
/// Returns a fully-loaded [`MlsGroupHandle`] for the new group on
/// success, or an [`MlsGroupError`] on:
///
/// - `MlsGroupError::BadSignature` — outer envelope sig failed
///   verification under `sender_ipk`. Spec §12.7.
/// - `MlsGroupError::BadCipherSuite` — the embedded `Welcome`'s
///   cipher suite is not `0x0003`. Spec §0.
/// - `MlsGroupError::Codec` — the `welcome_blob` bytes don't
///   parse as a TLS-encoded `Welcome`.
/// - `MlsGroupError::OpenMls(...)` — openmls rejected the welcome
///   (no matching KP, joiner secret invalid, …).
///
/// **Out-of-band gating** (the "is sender_ipk a contact?" check
/// from spec §3.3) is *not* in this function — it's the caller's
/// responsibility to surface a UI prompt before invoking
/// `process_welcome`. The anti-abuse / contact-first gating spec
/// (HANDOFF priority 2) will plug in there.
pub fn process_welcome(
    provider: &PromtuzMlsProvider, envelope: &WelcomeEnvelopeP,
) -> Result<MlsGroupHandle> {
    // ---------------------------------------------------------
    // 1. Verify outer sig.
    // ---------------------------------------------------------
    let transcript = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &envelope.group_id.0,
        &envelope.sender_ipk.0,
        &envelope.recipient_ipk.0,
        &envelope.kp_ref_used.0,
        &envelope.welcome_blob.0,
    );
    let verifying_key =
        VerifyingKey::from_bytes(&envelope.sender_ipk.0).map_err(|e| {
            MlsGroupError::Internal(format!("sender_ipk is not a valid Ed25519 key: {e}"))
        })?;
    let sig = Signature::from_bytes(&envelope.sender_sig.0);
    // Use `verify_strict` to reject non-canonical signatures and
    // small-order R values. Mirrors the discipline already in place on
    // the relay side (`relay/src/dht/mls_*`).
    verifying_key
        .verify_strict(&transcript, &sig)
        .map_err(|_| MlsGroupError::BadSignature)?;

    // ---------------------------------------------------------
    // 2. Deserialise as `MlsMessageIn` (the openmls 0.8 outer
    //    framing) and extract the inner `Welcome` body.
    //
    //    Per the spec §3.1 prose, the wire field is "TLS-encoded
    //    `openmls::Welcome`". We diverge: the bytes here are the
    //    full `MlsMessage` framing (`MlsMessageOut`/`MlsMessageIn`),
    //    which prefixes the version byte to the body. This is
    //    forced by openmls 0.8 gating `MlsMessageOut::into_welcome`
    //    behind `#[cfg(any(test, feature = "test-utils"))]` —
    //    production code cannot extract the naked Welcome at
    //    encode time. Functionally identical for security purposes;
    //    documented here so a future spec review can update §3.1.
    // ---------------------------------------------------------
    let mls_msg = MlsMessageIn::tls_deserialize_exact(&envelope.welcome_blob.0)
        .map_err(MlsGroupError::from_codec)?;
    let welcome = match mls_msg.extract() {
        MlsMessageBodyIn::Welcome(w) => w,
        other => {
            return Err(MlsGroupError::Internal(format!(
                "welcome_blob does not carry a Welcome body (got {other:?})"
            )));
        }
    };

    // ---------------------------------------------------------
    // 3. Hand to openmls — it'll look up the matching KP bundle
    //    by its hash_ref and decrypt the joiner secret.
    //
    //    Default join config matches openmls's own defaults
    //    (PrivateMessage out, mixed wire-format on incoming).
    // ---------------------------------------------------------
    let join_config = MlsGroupJoinConfig::default();
    let staged = StagedWelcome::new_from_welcome(provider, &join_config, welcome, None)
        .map_err(MlsGroupError::from_openmls)?;

    let mls_group = staged.into_group(provider).map_err(MlsGroupError::from_openmls)?;
    let handle = MlsGroupHandle::wrap(mls_group);

    // Cross-check the inner credential identities against the wire
    // envelope.
    //
    // The inner openmls `BasicCredential::identity` of the sender's
    // and recipient's leaves in the new group MUST match the wire
    // envelope's claimed IPKs. A credential-identity smuggling
    // attack would otherwise let a malicious sender publish a KP
    // claiming `record.ipk = Bob` but internally addressing the
    // openmls credential to `Alice`; the local Contact-store would
    // end up paired with a group whose MLS-layer membership
    // disagrees on who's at the other end.
    //
    // We require: at least one member's identity matches
    // `envelope.sender_ipk` AND at least one matches
    // `envelope.recipient_ipk`. We do NOT bound the total membership
    // (multi-party Welcomes carry N>=2 members; spec §4.4).
    //
    // Sender-/recipient-IPK address checks are done by the caller
    // (`process_welcome_inbound` in `api::messaging`) which knows the
    // `self_ipk` global.
    let recipient_ipk: [u8; 32] = envelope.recipient_ipk.0;
    let sender_ipk: [u8; 32] = envelope.sender_ipk.0;
    let mut saw_recipient = false;
    let mut saw_sender = false;
    for m in handle.members() {
        let id = m.credential.serialized_content();
        if id == recipient_ipk.as_slice() {
            saw_recipient = true;
        }
        if id == sender_ipk.as_slice() {
            saw_sender = true;
        }
    }
    if !saw_recipient {
        return Err(MlsGroupError::Internal(
            "Welcome's inner credentials lack recipient_ipk identity (smuggling?)".into(),
        ));
    }
    if !saw_sender {
        return Err(MlsGroupError::Internal(
            "Welcome's inner credentials lack sender_ipk identity (smuggling?)".into(),
        ));
    }

    Ok(handle)
}

#[cfg(test)]
mod tests {
    // Note: deliberately avoid `use super::*` to skip pulling in
    // ed25519_dalek's `Signature` type at module scope — openmls's
    // prelude also exports a `Signature` (an MLS sig) and the two
    // would conflict.
    use super::make_welcome_envelope;
    use super::process_welcome;
    use super::WelcomeEnvelopeP;
    use crate::db::mls::apply_mls_migrations;
    use crate::mls::group::mls_message_from_bytes;
    use crate::mls::group::mls_message_to_bytes;
    use crate::mls::group::MlsGroupHandle;
    use crate::mls::group::PROMTUZ_CIPHERSUITE;
    use crate::mls::provider::PromtuzMlsProvider;
    use crate::mls::types::MlsGroupError;
    use common::proto::mls_wire::welcome_envelope_signing_input;
    use common::proto::mls_wire::MLS_WIRE_VERSION;
    use ed25519_dalek::Signature as DalekSignature;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::Verifier;
    use ed25519_dalek::VerifyingKey;
    use openmls::prelude::BasicCredential;
    use openmls::prelude::Capabilities;
    use openmls::prelude::CredentialWithKey;
    use openmls::prelude::KeyPackage;
    use openmls::prelude::ProcessedMessageContent;
    use openmls::prelude::SignatureScheme;
    use parking_lot::Mutex;
    use rusqlite::Connection;
    use std::sync::Arc;

    fn build_provider() -> PromtuzMlsProvider {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        PromtuzMlsProvider::new(Arc::new(Mutex::new(conn)))
    }

    /// Minimal party fixture: deterministic IPK key + a fresh
    /// openmls `SignatureKeyPair` (the leaf signer). Mirrors
    /// `group::tests::Party` but exposes the IPK SigningKey so we
    /// can sign welcome envelopes with it.
    struct Party {
        ipk_sk: SigningKey,
        ipk: [u8; 32],
        sig_kp: openmls_basic_credential::SignatureKeyPair,
    }

    impl Party {
        fn new(provider: &PromtuzMlsProvider, seed: u8) -> Self {
            let ipk_sk = SigningKey::from_bytes(&[seed; 32]);
            let ipk = ipk_sk.verifying_key().to_bytes();
            let sig_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
                .expect("sig kp");
            sig_kp.store(provider.storage()).expect("store sig kp");
            Self { ipk_sk, ipk, sig_kp }
        }
    }

    /// Build a fresh KP for `party` and persist its bundle locally.
    fn make_kp(provider: &PromtuzMlsProvider, party: &Party) -> KeyPackage {
        let credential = BasicCredential::new(party.ipk.to_vec());
        let cwk = CredentialWithKey {
            credential: credential.into(),
            signature_key: party.sig_kp.public().into(),
        };
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(Capabilities::new(
                None,
                Some(&[PROMTUZ_CIPHERSUITE]),
                None,
                None,
                None,
            ))
            .build(PROMTUZ_CIPHERSUITE, provider, &party.sig_kp, cwk)
            .expect("build kp");
        bundle.key_package().clone()
    }

    /// Walk Alice → adds Bob → returns the `(welcome_envelope,
    /// alice_group)` for the test to reuse.
    fn alice_invites_bob(
        provider_a: &PromtuzMlsProvider, alice: &Party, provider_b: &PromtuzMlsProvider,
        bob: &Party, gid: [u8; 32],
    ) -> (WelcomeEnvelopeP, MlsGroupHandle) {
        let mut alice_group = MlsGroupHandle::create(
            provider_a,
            &alice.sig_kp,
            &alice.ipk,
            alice.sig_kp.public(),
            &gid,
        )
        .expect("create");
        let bob_kp = make_kp(provider_b, bob);
        // Save the kp_ref before consuming the kp into add_members.
        // The provider's `crypto()` is reached via the
        // `OpenMlsProvider` trait method, not the inherent method
        // (which is private).
        use openmls_traits::OpenMlsProvider;
        let kp_ref = bob_kp
            .hash_ref(provider_b.crypto())
            .expect("kp_ref")
            .as_slice()
            .to_vec();
        let (_commit, welcome) = alice_group
            .add_members(provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add bob");
        alice_group.merge_pending_commit(provider_a).expect("merge");

        let mut kp_ref_arr = [0u8; 32];
        let copy = kp_ref.len().min(32);
        kp_ref_arr[..copy].copy_from_slice(&kp_ref[..copy]);

        let env = make_welcome_envelope(
            welcome,
            gid,
            alice.ipk,
            bob.ipk,
            kp_ref_arr,
            &alice.ipk_sk,
        )
        .expect("make welcome");
        (env, alice_group)
    }

    // -------------------------------------------------------------
    // Test 1: Welcome envelope sig verifies.
    // -------------------------------------------------------------
    #[test]
    fn welcome_envelope_sig_verifies_under_sender_ipk() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let (env, _) = alice_invites_bob(&provider_a, &alice, &provider_b, &bob, [0xAA; 32]);

        // Verify the sig manually using the published transcript helper.
        let transcript = welcome_envelope_signing_input(
            MLS_WIRE_VERSION,
            &env.group_id.0,
            &env.sender_ipk.0,
            &env.recipient_ipk.0,
            &env.kp_ref_used.0,
            &env.welcome_blob.0,
        );
        let vk = VerifyingKey::from_bytes(&env.sender_ipk.0).expect("vk");
        let sig = DalekSignature::from_bytes(&env.sender_sig.0);
        assert!(vk.verify(&transcript, &sig).is_ok());
    }

    // -------------------------------------------------------------
    // Test 2: Welcome envelope with bad sig is rejected.
    // -------------------------------------------------------------
    #[test]
    fn welcome_envelope_bad_sig_is_rejected() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let (mut env, _) = alice_invites_bob(&provider_a, &alice, &provider_b, &bob, [0xAA; 32]);

        // Flip a bit in the sig.
        let mut sig_bytes = env.sender_sig.0;
        sig_bytes[0] ^= 0xFF;
        env.sender_sig = sig_bytes.into();

        let r = process_welcome(&provider_b, &env);
        assert!(matches!(r, Err(MlsGroupError::BadSignature)));
    }

    // -------------------------------------------------------------
    // Test 3: Wrong recipient_ipk is rejected (sig won't verify
    // because recipient_ipk is bound).
    // -------------------------------------------------------------
    #[test]
    fn welcome_addressed_to_wrong_recipient_is_rejected() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let (mut env, _) = alice_invites_bob(&provider_a, &alice, &provider_b, &bob, [0xAA; 32]);

        // Overwrite recipient_ipk; sig was bound to the original
        // value, so it must fail verification.
        let mallory_ipk = SigningKey::from_bytes(&[3u8; 32])
            .verifying_key()
            .to_bytes();
        env.recipient_ipk = mallory_ipk.into();

        let r = process_welcome(&provider_b, &env);
        assert!(matches!(r, Err(MlsGroupError::BadSignature)));
    }

    // -------------------------------------------------------------
    // Test 4: process_welcome on a valid envelope materialises a
    // joined group.
    // -------------------------------------------------------------
    #[test]
    fn process_welcome_materialises_group() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let gid = [0xBB; 32];
        let (env, alice_group) = alice_invites_bob(&provider_a, &alice, &provider_b, &bob, gid);

        let bob_group = process_welcome(&provider_b, &env).expect("process");
        assert_eq!(bob_group.group_id(), gid);
        assert_eq!(bob_group.epoch(), alice_group.epoch());
        assert_eq!(bob_group.member_count(), alice_group.member_count());
    }

    // -------------------------------------------------------------
    // Test 5: After process_welcome, founder + joiner can exchange
    // application messages.
    // -------------------------------------------------------------
    #[test]
    fn after_process_welcome_application_messages_round_trip() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let gid = [0xBB; 32];
        let (env, mut alice_group) =
            alice_invites_bob(&provider_a, &alice, &provider_b, &bob, gid);

        let mut bob_group = process_welcome(&provider_b, &env).expect("process");

        // Alice → Bob.
        let plaintext = b"welcome bob, can you read me?";
        let alice_msg = alice_group
            .create_application_message(&provider_a, &alice.sig_kp, plaintext)
            .expect("encrypt");
        let bytes = mls_message_to_bytes(&alice_msg).expect("ser");
        let in_msg = mls_message_from_bytes(&bytes).expect("deser");
        let proto = in_msg.try_into_protocol_message().expect("proto");
        let content = bob_group
            .process_incoming(&provider_b, proto)
            .expect("process app");
        match content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext);
            }
            other => panic!("expected app msg, got {other:?}"),
        }

        // Bob → Alice.
        let plaintext_b = b"yes alice, loud and clear";
        let bob_msg = bob_group
            .create_application_message(&provider_b, &bob.sig_kp, plaintext_b)
            .expect("bob encrypt");
        let bytes = mls_message_to_bytes(&bob_msg).expect("ser");
        let in_msg = mls_message_from_bytes(&bytes).expect("deser");
        let proto = in_msg.try_into_protocol_message().expect("proto");
        let content = alice_group
            .process_incoming(&provider_a, proto)
            .expect("alice process");
        match content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext_b);
            }
            other => panic!("expected app msg, got {other:?}"),
        }
    }
}
