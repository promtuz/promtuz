//! `MlsGroupHandle` — high-level wrapper over `openmls::group::MlsGroup`
//! exposing the promtuz-flavored group API.
//!
//! # Scope
//!
//! - **Lifecycle**: create / add / remove / self-update / leave (spec
//!   §4).
//! - **Application messaging**: encrypt-out, decrypt-in (spec §3.1
//!   inner MLS message wrapping; outer envelope is in `welcome.rs`
//!   and the `messaging.rs` wiring).
//! - **Export secret** for SFrame integration (spec §10.2).
//!
//! # Cipher suite pin
//!
//! Hard-pinned to
//! `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519` (suite
//! `0x0003`) per spec §0. Note that openmls 0.8's
//! `MlsGroupCreateConfig::default()` selects a *different* suite
//! (`MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`) — we override at
//! construction time. **Mismatch on this would silently shift the
//! AEAD from ChaCha20-Poly1305 to AES-128-GCM, breaking the spec.**
//! Pinned in [`PROMTUZ_CIPHERSUITE`].
//!
//! # Group ID shape
//!
//! Spec §4.1 mandates `group_id` is 32 B. `openmls::group::GroupId`
//! accepts arbitrary length; we constrain at construction by passing
//! `&[u8; 32]` and convert via `GroupId::from_slice`.
//!
//! # Signer caveats
//!
//! `openmls_traits::signatures::Signer` is intentionally narrow: it
//! exposes `sign(&self, payload) -> Vec<u8>` and `signature_scheme`
//! only — **no public-key getter**. Therefore [`Self::create`] takes
//! the leaf signing public key as an explicit argument; the caller is
//! responsible for keeping it consistent with the signer's secret
//! half. Both [`super::signer::Ed25519Signer::public_key`] and
//! `openmls_basic_credential::SignatureKeyPair::public()` expose the
//! 32-byte slice the constructor wants.
//!
//! design-doc: `misc/specs/MLS.md` §0 (cipher suite), §3.1 (envelope
//! shape), §4 (lifecycle), §10 (calls / export_secret).

// All public items here are consumed by `messaging.rs`; the cdylib
// compiler can't see across the JNI boundary so it flags them as
// dead. Module-wide allow-lint matches the pattern in `provider.rs`.
#![allow(dead_code)]

use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;
use openmls_traits::signatures::Signer;
use openmls_traits::OpenMlsProvider;

use super::provider::PromtuzMlsProvider;
use super::types::MlsGroupError;

/// The single cipher suite we use across promtuz, pinned per spec §0.
pub const PROMTUZ_CIPHERSUITE: Ciphersuite =
    Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519;

/// Convenience type alias — every result in this module funnels
/// failures through [`MlsGroupError`].
type Result<T> = std::result::Result<T, MlsGroupError>;

/// Promtuz-flavored handle over an openmls group.
///
/// Holds the underlying `MlsGroup` by value. Persistence happens
/// inside openmls (it calls into the [`PromtuzStorageProvider`] on
/// every state-mutating operation) so this struct does *not* need to
/// re-persist on its own.
///
/// **Not `Clone`** — an MLS group is a stateful crypto object and
/// cloning would violate the one-mutator invariant.
#[derive(Debug)]
pub struct MlsGroupHandle {
    inner: MlsGroup,
}

impl MlsGroupHandle {
    /// Construct a fresh group with the caller as the founding member.
    ///
    /// `own_ipk` is the caller's long-term Ed25519 IPK; it becomes the
    /// `BasicCredential::identity` (spec §5.2). `signer` is the
    /// **leaf** signing key (distinct from IPK, see `signer.rs`);
    /// `leaf_signing_public` is its 32-byte verifying half (the
    /// `Signer` trait can't expose this directly — see module docs).
    ///
    /// `group_id` is the 32-byte promtuz group identifier (spec §4.1).
    ///
    /// **Cipher suite is pinned** to [`PROMTUZ_CIPHERSUITE`].
    pub fn create<S: Signer>(
        provider: &PromtuzMlsProvider, signer: &S, own_ipk: &[u8; 32],
        leaf_signing_public: &[u8], group_id: &[u8; 32],
    ) -> Result<Self> {
        let credential = BasicCredential::new(own_ipk.to_vec());
        let credential_with_key = CredentialWithKey {
            credential: credential.into(),
            signature_key: leaf_signing_public.to_vec().into(),
        };

        let create_config = MlsGroupCreateConfig::builder()
            .ciphersuite(PROMTUZ_CIPHERSUITE)
            // `use_ratchet_tree_extension(true)` ships the ratchet tree
            // inside the GroupInfo / Welcome rather than out-of-band.
            // Without it joiners would require a separately-conveyed
            // RatchetTreeIn — we don't have that channel today (spec
            // §4.2 step 8 implicitly assumes the tree is in-band).
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_with_group_id(
            provider,
            signer,
            &create_config,
            GroupId::from_slice(group_id),
            credential_with_key,
        )
        .map_err(MlsGroupError::from_openmls)?;

        Ok(Self { inner: mls_group })
    }

    /// Load an existing group from storage. Used after a libcore
    /// restart: openmls reads back the persisted state via the
    /// `StorageProvider`. Returns `Ok(None)` if no group with
    /// `group_id` is stored.
    pub fn load(provider: &PromtuzMlsProvider, group_id: &[u8; 32]) -> Result<Option<Self>> {
        let gid = GroupId::from_slice(group_id);
        let loaded =
            MlsGroup::load(provider.storage(), &gid).map_err(MlsGroupError::Storage)?;
        Ok(loaded.map(|inner| Self { inner }))
    }

    /// Add members to the group via their KeyPackages.
    ///
    /// Per openmls 0.8: `MlsGroup::add_members` returns
    /// `(commit, welcome, Option<GroupInfo>)`. We expose only
    /// `(commit, welcome)` — the optional GroupInfo is reserved for
    /// external-commit rejoin (spec §8.2), not used in v1.
    ///
    /// **The caller must merge the pending commit afterwards** via
    /// [`Self::merge_pending_commit`]. Until then the group is in
    /// `MlsGroupState::PendingCommit` and openmls rejects further
    /// mutations.
    pub fn add_members<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, new_members: &[KeyPackage],
    ) -> Result<(MlsMessageOut, MlsMessageOut)> {
        let (commit, welcome, _group_info) = self
            .inner
            .add_members(provider, signer, new_members)
            .map_err(MlsGroupError::from_openmls)?;
        Ok((commit, welcome))
    }

    /// Remove members by leaf index.
    ///
    /// Per openmls 0.8: returns
    /// `(commit, Option<welcome>, Option<GroupInfo>)`. The Welcome
    /// is `Some` only if there are also pending Add proposals
    /// (mixed-batch); for a pure-remove call it's `None`. We surface
    /// only the commit.
    ///
    /// Caller must merge pending commit afterwards.
    pub fn remove_members<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, members: &[LeafNodeIndex],
    ) -> Result<MlsMessageOut> {
        let (commit, _welcome, _group_info) = self
            .inner
            .remove_members(provider, signer, members)
            .map_err(MlsGroupError::from_openmls)?;
        Ok(commit)
    }

    /// Rotate own leaf key (Update commit, spec §4.6 — PCS).
    ///
    /// The new leaf's HPKE init key + signature key are derived
    /// internally by openmls. The caller does *not* supply a fresh
    /// signer. The Commit must be fanned out to all other members
    /// and merged locally via [`Self::merge_pending_commit`].
    pub fn self_update<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S,
    ) -> Result<MlsMessageOut> {
        let bundle = self
            .inner
            .self_update(provider, signer, LeafNodeParameters::default())
            .map_err(MlsGroupError::from_openmls)?;
        let (commit, _welcome, _group_info) = bundle.into_contents();
        Ok(commit)
    }

    /// Self-removal (spec §4.7).
    ///
    /// **Important asymmetry vs. spec prose**: `MlsGroup::leave_group`
    /// in openmls 0.8 returns a *Remove proposal*, **not** a Commit.
    /// The remaining members must commit it (via their own
    /// `commit_to_pending_proposals`). That diverges slightly from
    /// §4.7 ("Alice commits her own removal"); we follow openmls's
    /// actual API and document it here.
    pub fn leave<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S,
    ) -> Result<MlsMessageOut> {
        self.inner
            .leave_group(provider, signer)
            .map_err(MlsGroupError::from_openmls)
    }

    /// Encrypt an application message for the group's current epoch.
    ///
    /// Returns an `MlsMessageOut` (a `PrivateMessage` framing). The
    /// caller TLS-serialises via [`mls_message_to_bytes`] before
    /// stuffing into `MlsApplicationEnvelopeP::mls_message`.
    pub fn create_application_message<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, plaintext: &[u8],
    ) -> Result<MlsMessageOut> {
        self.inner
            .create_message(provider, signer, plaintext)
            .map_err(MlsGroupError::from_openmls)
    }

    /// Process an incoming MLS message.
    ///
    /// Returns `ProcessedMessageContent` — application payloads,
    /// proposals, or staged commits. **Caller must** then:
    /// - Surface `ApplicationMessage` content to the UI.
    /// - For `StagedCommitMessage`: call
    ///   [`Self::merge_staged_commit`] to advance the local epoch.
    /// - For `ProposalMessage`: queue via openmls's
    ///   `store_pending_proposal`.
    pub fn process_incoming(
        &mut self, provider: &PromtuzMlsProvider, message: ProtocolMessage,
    ) -> Result<ProcessedMessageContent> {
        let processed = self
            .inner
            .process_message(provider, message)
            .map_err(MlsGroupError::from_openmls)?;
        Ok(processed.into_content())
    }

    /// Merge a *staged commit* (the result of processing a peer's
    /// commit) into our local state. Advances the epoch.
    pub fn merge_staged_commit(
        &mut self, provider: &PromtuzMlsProvider, staged: StagedCommit,
    ) -> Result<()> {
        self.inner
            .merge_staged_commit(provider, staged)
            .map_err(MlsGroupError::from_openmls)
    }

    /// Merge a *pending commit* (one we built via
    /// [`Self::add_members`] / [`Self::remove_members`] /
    /// [`Self::self_update`]) into our local state. Advances the
    /// epoch.
    pub fn merge_pending_commit(&mut self, provider: &PromtuzMlsProvider) -> Result<()> {
        self.inner
            .merge_pending_commit(provider)
            .map_err(MlsGroupError::from_openmls)
    }

    /// Current group epoch as a plain `u64`.
    pub fn epoch(&self) -> u64 {
        self.inner.epoch().as_u64()
    }

    /// Current group ID as a 32-byte array.
    ///
    /// Returns the first 32 bytes of the underlying `GroupId` (the
    /// rest is dropped). Spec §4.1 fixes IDs to 32 B; this defensive
    /// truncation handles loaded-from-disk groups that may have
    /// shorter values from a future tooling that admitted shorter
    /// IDs — they get zero-padded rather than panicking.
    pub fn group_id(&self) -> [u8; 32] {
        let slice = self.inner.group_id().as_slice();
        let mut out = [0u8; 32];
        let copy_len = slice.len().min(32);
        out[..copy_len].copy_from_slice(&slice[..copy_len]);
        out
    }

    /// Number of members currently in the group.
    pub fn member_count(&self) -> usize {
        self.inner.members().count()
    }

    /// Iterate members. Returned items expose `index: LeafNodeIndex`
    /// and `credential: Credential`; the
    /// `BasicCredential::identity` carries each member's IPK bytes.
    pub fn members(&self) -> impl Iterator<Item = Member> + '_ {
        self.inner.members()
    }

    /// Find a member by their IPK. Returns the leaf index, or `None`
    /// if no member's `BasicCredential::identity` matches.
    pub fn member_index_by_ipk(&self, ipk: &[u8; 32]) -> Option<LeafNodeIndex> {
        self.inner
            .members()
            .find(|m| m.credential.serialized_content() == ipk.as_slice())
            .map(|m| m.index)
    }

    /// Export an MLS exporter secret for SFrame / call key derivation
    /// (spec §10.2).
    pub fn export_secret(
        &self, provider: &PromtuzMlsProvider, label: &str, context: &[u8], length: usize,
    ) -> Result<Vec<u8>> {
        self.inner
            .export_secret(provider.crypto(), label, context, length)
            .map_err(MlsGroupError::from_openmls)
    }

    /// Drop the persisted state of this group from the openmls storage
    /// provider. Used by `lazy_create_group` to roll back the group
    /// when the Welcome publish fails quorum —
    /// otherwise the contact's `mls_group_id` would dangle against an
    /// orphan local group that the recipient never joined, and the
    /// sender's stash would slowly accumulate dead group state.
    ///
    /// Mirrors `MlsGroup::delete`: deletes group config, leaf indices,
    /// epoch secrets, message secrets, all PSK secrets, leaf-node
    /// list, group state, and queued proposals. The leaf signing key
    /// is left in storage because openmls's `delete` doesn't manage it
    /// (callers can re-use it for a retry); the public-facing impact
    /// is "the group is gone; the next send to this peer will
    /// lazy-create a fresh one".
    pub fn delete(&mut self, provider: &PromtuzMlsProvider) -> Result<()> {
        self.inner
            .delete(provider.storage())
            .map_err(MlsGroupError::Storage)
    }

    // ------------------------------------------------------------
    // Internal: wrap/unwrap for sibling modules (welcome.rs).
    // ------------------------------------------------------------

    /// Wrap an existing `MlsGroup` (used by `welcome.rs` after
    /// `StagedWelcome::into_group`).
    pub(crate) fn wrap(inner: MlsGroup) -> Self {
        Self { inner }
    }
}

/// TLS-serialise an `MlsMessageOut` for stuffing into
/// `MlsApplicationEnvelopeP::mls_message`. Mirrors the spec §3.1
/// transcript discipline: we never invent our own framing for the
/// inner MLS bytes — openmls owns it.
#[allow(dead_code)] // messaging.rs caller.
pub fn mls_message_to_bytes(msg: &MlsMessageOut) -> Result<Vec<u8>> {
    msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)
}

/// TLS-deserialise an `MlsMessageIn` from envelope bytes. Returned
/// type carries the wire-format tag and gives the caller access to
/// `extract()` / `try_into_protocol_message()` to dispatch into
/// `process_incoming`.
#[allow(dead_code)] // messaging.rs caller.
pub fn mls_message_from_bytes(bytes: &[u8]) -> Result<MlsMessageIn> {
    use openmls::prelude::tls_codec::Deserialize as _;
    MlsMessageIn::tls_deserialize_exact(bytes).map_err(MlsGroupError::from_codec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use openmls::prelude::tls_codec::Deserialize as _;
    use parking_lot::Mutex;
    use rusqlite::Connection;
    use std::sync::Arc;

    /// Build a fresh in-memory provider for tests.
    fn build_provider() -> PromtuzMlsProvider {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        PromtuzMlsProvider::new(Arc::new(Mutex::new(conn)))
    }

    /// Test fixture: deterministic IPK + a fresh
    /// `SignatureKeyPair` (the leaf signer). The leaf signing key is
    /// random per call (openmls's `SignatureKeyPair::new` doesn't
    /// take a seed). For test stability we don't care about
    /// determinism across runs — we care that within a single test
    /// the same signer is reused for create + add operations.
    struct Party {
        ipk: [u8; 32],
        sig_kp: openmls_basic_credential::SignatureKeyPair,
    }

    impl Party {
        fn new(provider: &PromtuzMlsProvider, ipk_seed: u8) -> Self {
            // IPK is deterministic; leaf signing key is random — the
            // separation matches spec §5.2.
            let ipk = ed25519_dalek::SigningKey::from_bytes(&[ipk_seed; 32])
                .verifying_key()
                .to_bytes();
            let sig_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
                .expect("sig kp");
            sig_kp.store(provider.storage()).expect("store sig kp");
            Self { ipk, sig_kp }
        }
    }

    /// Build a fresh KeyPackage for `party` and persist its bundle in
    /// `provider`'s storage. The KeyPackage itself ships across to a
    /// counterparty's group; the bundle (init+enc keys) stays local.
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

    /// Helper: Alice creates a 1-member group.
    fn create_group(provider: &PromtuzMlsProvider, party: &Party, gid: &[u8; 32]) -> MlsGroupHandle {
        MlsGroupHandle::create(
            provider,
            &party.sig_kp,
            &party.ipk,
            party.sig_kp.public(),
            gid,
        )
        .expect("create group")
    }

    // -------------------------------------------------------------
    // Test 1: Create a 1-member group.
    // -------------------------------------------------------------
    #[test]
    fn create_one_member_group() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let gid = [0xAA; 32];
        let group = create_group(&provider, &alice, &gid);
        assert_eq!(group.group_id(), gid);
        assert_eq!(group.epoch(), 0);
        assert_eq!(group.member_count(), 1);
    }

    // -------------------------------------------------------------
    // Test 2: Add a member (yields commit + welcome).
    // -------------------------------------------------------------
    #[test]
    fn add_member_yields_commit_and_welcome() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let mut group = create_group(&provider_a, &alice, &[0xAA; 32]);
        let bob_kp = make_kp(&provider_b, &bob);
        let (_commit, _welcome) = group
            .add_members(&provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add member");
        group.merge_pending_commit(&provider_a).expect("merge");
        assert_eq!(group.epoch(), 1);
        assert_eq!(group.member_count(), 2);
    }

    /// Helper: extract a `Welcome` from an `MlsMessageOut` by
    /// round-tripping through tls_codec → `MlsMessageIn` →
    /// `MlsMessageBodyIn::Welcome`. We can't use
    /// `MlsMessageOut::into_welcome()` directly because in openmls
    /// 0.8 it's gated behind `#[cfg(any(test, feature = "test-utils"))]`
    /// — `cfg(test)` only fires for openmls *itself*, not for its
    /// dependents.
    fn extract_welcome_via_tls(msg: MlsMessageOut) -> Welcome {
        let bytes = msg.tls_serialize_detached().expect("ser");
        let in_msg = MlsMessageIn::tls_deserialize_exact(&bytes).expect("deser");
        match in_msg.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            other => panic!("expected Welcome body, got {other:?}"),
        }
    }

    // -------------------------------------------------------------
    // Test 3: Founder + new member exchange application messages.
    // -------------------------------------------------------------
    #[test]
    fn add_then_application_message_round_trip() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);

        // Alice creates and adds Bob.
        let mut alice_group = create_group(&provider_a, &alice, &[0xAA; 32]);
        let bob_kp = make_kp(&provider_b, &bob);
        let (_commit, welcome) = alice_group
            .add_members(&provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add bob");
        alice_group.merge_pending_commit(&provider_a).expect("merge");

        // Bob processes Welcome.
        let welcome_msg = extract_welcome_via_tls(welcome);
        let join_config = MlsGroupJoinConfig::default();
        let staged = StagedWelcome::new_from_welcome(
            &provider_b, &join_config, welcome_msg, None,
        )
        .expect("staged");
        let mut bob_group = MlsGroupHandle::wrap(staged.into_group(&provider_b).expect("into"));

        assert_eq!(bob_group.epoch(), alice_group.epoch());
        assert_eq!(bob_group.group_id(), alice_group.group_id());

        // Alice → Bob.
        let plaintext = b"hello bob";
        let alice_msg = alice_group
            .create_application_message(&provider_a, &alice.sig_kp, plaintext)
            .expect("encrypt");
        let bytes = mls_message_to_bytes(&alice_msg).expect("ser");
        let on_bob = mls_message_from_bytes(&bytes).expect("deser");
        let proto = on_bob.try_into_protocol_message().expect("proto");
        let content = bob_group.process_incoming(&provider_b, proto).expect("process");
        match content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext);
            }
            other => panic!("expected app msg, got {other:?}"),
        }

        // Bob → Alice. Bob's signer is bob.sig_kp (same provider, same kp).
        let plaintext_b = b"hi alice";
        let bob_msg = bob_group
            .create_application_message(&provider_b, &bob.sig_kp, plaintext_b)
            .expect("bob encrypt");
        let bytes = mls_message_to_bytes(&bob_msg).expect("ser");
        let on_alice = mls_message_from_bytes(&bytes).expect("deser");
        let content = alice_group
            .process_incoming(
                &provider_a,
                on_alice.try_into_protocol_message().expect("proto"),
            )
            .expect("alice process");
        match content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext_b);
            }
            other => panic!("expected app msg, got {other:?}"),
        }
    }

    // -------------------------------------------------------------
    // Test 4: Remove a member.
    // -------------------------------------------------------------
    #[test]
    fn remove_member_advances_epoch_and_excludes_removed() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);

        let mut alice_group = create_group(&provider_a, &alice, &[0xAA; 32]);
        let bob_kp = make_kp(&provider_b, &bob);
        let (_c, welcome) = alice_group
            .add_members(&provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add");
        alice_group.merge_pending_commit(&provider_a).expect("merge");

        let welcome_msg = extract_welcome_via_tls(welcome);
        let join_config = MlsGroupJoinConfig::default();
        let staged = StagedWelcome::new_from_welcome(
            &provider_b, &join_config, welcome_msg, None,
        )
        .expect("staged");
        let mut bob_group = MlsGroupHandle::wrap(staged.into_group(&provider_b).expect("into"));
        let pre = alice_group.epoch();

        // Alice removes Bob.
        let bob_idx = alice_group
            .member_index_by_ipk(&bob.ipk)
            .expect("bob is a member");
        let _commit = alice_group
            .remove_members(&provider_a, &alice.sig_kp, &[bob_idx])
            .expect("remove");
        alice_group.merge_pending_commit(&provider_a).expect("merge");

        assert_eq!(alice_group.epoch(), pre + 1);
        assert_eq!(alice_group.member_count(), 1);

        // Bob (still at pre-remove epoch) can't decrypt new
        // messages.
        let after = alice_group
            .create_application_message(&provider_a, &alice.sig_kp, b"after-remove")
            .expect("encrypt");
        let bytes = mls_message_to_bytes(&after).expect("ser");
        let in_msg = mls_message_from_bytes(&bytes).expect("deser");
        let proto = in_msg.try_into_protocol_message().expect("proto");
        let result = bob_group.process_incoming(&provider_b, proto);
        assert!(result.is_err(), "removed Bob can't decrypt new-epoch");
    }

    // -------------------------------------------------------------
    // Test 5: Self-update.
    // -------------------------------------------------------------
    #[test]
    fn self_update_advances_epoch() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let mut alice_group = create_group(&provider_a, &alice, &[0xAA; 32]);
        let bob_kp = make_kp(&provider_b, &bob);
        let (_c, _w) = alice_group
            .add_members(&provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add");
        alice_group.merge_pending_commit(&provider_a).expect("merge");
        let pre = alice_group.epoch();

        let _commit = alice_group
            .self_update(&provider_a, &alice.sig_kp)
            .expect("self_update");
        alice_group.merge_pending_commit(&provider_a).expect("merge");
        assert_eq!(alice_group.epoch(), pre + 1);
        assert_eq!(alice_group.member_count(), 2);
    }

    // -------------------------------------------------------------
    // Test 6: Leave produces a Remove proposal.
    // -------------------------------------------------------------
    #[test]
    fn leave_produces_remove_proposal() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let mut group = create_group(&provider, &alice, &[0xAA; 32]);
        let proposal = group.leave(&provider, &alice.sig_kp).expect("leave");
        // openmls 0.8: leave returns a Remove proposal (PublicMessage
        // by default). The wire format is *not* PrivateMessage.
        assert_ne!(
            mls_message_to_bytes(&proposal).expect("ser").len(),
            0,
            "proposal serialises to non-empty bytes"
        );
    }

    // -------------------------------------------------------------
    // Test 7: tls_codec round-trip stays consistent.
    // -------------------------------------------------------------
    #[test]
    fn application_round_trip_through_tls_codec() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let mut group = create_group(&provider, &alice, &[0xCC; 32]);
        let msg = group
            .create_application_message(&provider, &alice.sig_kp, b"alice-only")
            .expect("encrypt");
        let bytes = mls_message_to_bytes(&msg).expect("ser");
        let parsed = mls_message_from_bytes(&bytes).expect("deser");
        assert_eq!(parsed.wire_format(), WireFormat::PrivateMessage);
    }

    // -------------------------------------------------------------
    // Test 8: Cipher suite is fixed to 0x0003.
    // -------------------------------------------------------------
    #[test]
    fn ciphersuite_is_pinned() {
        assert_eq!(
            PROMTUZ_CIPHERSUITE,
            Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519
        );
        assert_eq!(PROMTUZ_CIPHERSUITE as u16, 0x0003);
    }

    // -------------------------------------------------------------
    // Test 9: Persistence round-trip (load reads back what create wrote).
    // -------------------------------------------------------------
    #[test]
    fn group_persists_via_storage_provider() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let gid = [0xDD; 32];
        let _group = create_group(&provider, &alice, &gid);
        // Drop the handle, reload via the same provider.
        drop(_group);
        let loaded = MlsGroupHandle::load(&provider, &gid).expect("load");
        assert!(loaded.is_some(), "group is persisted in storage");
        assert_eq!(loaded.unwrap().group_id(), gid);
    }

    // -------------------------------------------------------------
    // Test 10: export_secret returns bytes of the requested length.
    // -------------------------------------------------------------
    #[test]
    fn export_secret_returns_requested_length() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let group = create_group(&provider, &alice, &[0xEE; 32]);
        let secret = group
            .export_secret(&provider, "test-label", b"test-context", 32)
            .expect("export");
        assert_eq!(secret.len(), 32);
    }
}
