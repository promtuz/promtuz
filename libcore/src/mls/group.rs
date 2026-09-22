//! `MlsGroupHandle` — high-level wrapper over `openmls::group::MlsGroup`
//! exposing the promtuz-flavored group API.
//!
//! # Scope
//!
//! - **Lifecycle**: create / add / remove / self-update / leave.
//! - **Application messaging**: encrypt-out, decrypt-in (inner MLS
//!   message wrapping; outer envelope is in `welcome.rs` and the
//!   `messaging.rs` wiring).
//! - **Export secret** for SFrame integration.
//!
//! # Cipher suite pin
//!
//! Hard-pinned to
//! `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519` (suite
//! `0x0003`). Note that openmls 0.8's
//! `MlsGroupCreateConfig::default()` selects a *different* suite
//! (`MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`) — we override at
//! construction time. **Mismatch on this would silently shift the
//! AEAD from ChaCha20-Poly1305 to AES-128-GCM, breaking the spec.**
//! Pinned in [`PROMTUZ_CIPHERSUITE`].
//!
//! # Group ID shape
//!
//! `group_id` is fixed at 32 B. `openmls::group::GroupId`
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

// All public items here are consumed by `messaging.rs`; the cdylib
// compiler can't see across the JNI boundary so it flags them as
// dead. Module-wide allow-lint matches the pattern in `provider.rs`.
#![allow(dead_code)]

use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;
use openmls_traits::signatures::Signer;
use openmls_traits::OpenMlsProvider;
use serde::Deserialize;
use serde::Serialize;

use super::provider::PromtuzMlsProvider;
use super::types::MlsGroupError;

/// The single cipher suite used across promtuz.
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

/// Extension type carrying [`GroupMeta`] in the group context.
///
/// Private-use range (RFC 9420 §17.3). openmls only demands capability support
/// for extensions named in `RequiredCapabilities`, so an unknown one in the
/// context is carried by every implementation without negotiation.
const PROMTUZ_GROUP_META_EXT: u16 = 0xF100;

/// [`PROMTUZ_GROUP_META_EXT`] as openmls names it. Every KeyPackage must
/// declare support for this or it cannot be added to a group — RFC 9420
/// requires a joining leaf to support every extension in the group context.
pub const GROUP_META_EXTENSION: ExtensionType = ExtensionType::Unknown(PROMTUZ_GROUP_META_EXT);

/// What a group *is*, decided by whoever created it and carried in the MLS
/// group context.
///
/// A group of two and a 1:1 chat are the same shape on the wire — same MLS
/// group, same envelopes — so nothing observable tells a joiner which one they
/// were just Welcomed into. Guessing from the roster is wrong the moment a
/// group has exactly two members. The creator knows, so the creator says.
///
/// It lives in the group context rather than in the Welcome envelope for three
/// reasons: MLS signs the context, so a relay can neither strip it (turning a
/// group into a DM) nor forge one; it reaches the joiner inside the Welcome, so
/// there is no message that can arrive before it; and the relay parses none of
/// it, so this needed no wire version bump and no relay deploy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMeta {
    /// The group's name when it was founded. Later renames travel as
    /// `SystemEvent::Titled`; this is only the starting point.
    pub title:   String,
    /// Who founded it, and so who administers it.
    ///
    /// Carried here rather than inferred from whoever sent us the Welcome: a
    /// group whose state landed without a conversation is homed by the next
    /// message to arrive in it, and the sender of that message is whoever
    /// happened to speak first. Reading the owner from the context means every
    /// member agrees on it however they came to learn about the group.
    #[serde(with = "serde_bytes")]
    pub founder: [u8; 32],
}

/// A decrypted inbound message together with the member who wrote it.
///
/// `sender` is the identity bound to the authenticated MLS leaf that produced
/// this — the only authority on authorship inside a group. See
/// [`super::credential`] for what "bound" means and why a bare claim is not.
pub struct ProcessedInbound {
    pub sender:  [u8; 32],
    pub content: ProcessedMessageContent,
}

impl MlsGroupHandle {
    /// Construct a fresh group with the caller as the founding member.
    ///
    /// `signer` is the **leaf** signing key (distinct from IPK, see
    /// `signer.rs`); `credential_with_key` is that key's public half under the
    /// credential that binds it to the caller's identity — see
    /// [`super::credential::bound_credential`].
    ///
    /// `group_id` is the 32-byte promtuz group identifier. `meta` marks this a
    /// group chat rather than a 1:1 — see [`GroupMeta`]; `None` builds a pair.
    ///
    /// **Cipher suite is pinned** to [`PROMTUZ_CIPHERSUITE`].
    pub fn create<S: Signer>(
        provider: &PromtuzMlsProvider, signer: &S, credential_with_key: CredentialWithKey,
        group_id: &[u8; 32], meta: Option<&GroupMeta>,
    ) -> Result<Self> {
        let mut create_config = MlsGroupCreateConfig::builder()
            .ciphersuite(PROMTUZ_CIPHERSUITE)
            // Handshake framing stays opaque to the relay. Pinned rather than
            // inherited from the openmls default so it cannot drift.
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .padding_size(super::MLS_PADDING_SIZE)
            // `use_ratchet_tree_extension(true)` ships the ratchet tree
            // inside the GroupInfo / Welcome rather than out-of-band.
            // Without it joiners would require a separately-conveyed
            // RatchetTreeIn — we don't have that channel today.
            .use_ratchet_tree_extension(true);

        if let Some(meta) = meta {
            let bytes = postcard::to_allocvec(meta)
                .map_err(|e| MlsGroupError::Codec(e.to_string()))?;
            let ext = Extension::Unknown(PROMTUZ_GROUP_META_EXT, UnknownExtension(bytes));
            let exts = Extensions::single(ext)
                .map_err(|e| MlsGroupError::Codec(format!("group meta extension: {e}")))?;
            // The founder's own leaf has to declare the extension too, not just
            // the leaves it adds — RFC 9420 holds every member to the same bar,
            // including whoever put the extension there.
            create_config = create_config
                .with_group_context_extensions(exts)
                .capabilities(Capabilities::new(
                    None,
                    Some(&[PROMTUZ_CIPHERSUITE]),
                    Some(&[GROUP_META_EXTENSION]),
                    None,
                    None,
                ));
        }
        let create_config = create_config.build();

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
    /// external-commit rejoin, not used today.
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

    /// Rotate own leaf key (Update commit — PCS).
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

    /// Self-removal.
    ///
    /// **Important**: `MlsGroup::leave_group` in openmls 0.8 returns a
    /// *Remove proposal*, **not** a Commit. The remaining members must
    /// commit it (via their own `commit_to_pending_proposals`).
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
    ) -> Result<ProcessedInbound> {
        let processed = self
            .inner
            .process_message(provider, message)
            .map_err(MlsGroupError::from_openmls)?;
        // Read the author off the authenticated leaf before the content
        // consumes it. MLS proves which leaf produced this message, and the
        // leaf's credential proves whose it is; the outer envelope only
        // proves who handed it to the relay, and in a group those are
        // routinely different people. A leaf that proves nothing is nobody,
        // and nobody's messages are refused rather than attributed.
        let sender = match processed.sender() {
            Sender::Member(index) => self
                .inner
                .member_at(*index)
                .and_then(|m| super::credential::member_ipk(&m, self.is_group_chat())),
            _ => None,
        }
        .ok_or(MlsGroupError::UnboundSender)?;
        Ok(ProcessedInbound { sender, content: processed.into_content() })
    }

    /// The membership rule, applied on receipt. MLS lets any member commit any
    /// proposal; the founder-only policy the send path enforces in
    /// [`crate::groups`] is only as good as the client that sends, so every
    /// receiver re-checks it here: only the founder may add, or remove anyone
    /// who did not propose their own removal; a self-proposed removal — a leave
    /// — may be committed by anyone; and nothing else may touch the group, in
    /// particular its context extensions, which is where the founder is named.
    /// A pair group has no founder and its roster never changes.
    pub fn commit_is_permitted(
        &self, staged: &StagedCommit, author: [u8; 32],
    ) -> std::result::Result<(), &'static str> {
        use super::credential::leaf_node_ipk;
        let strict = self.is_group_chat();
        // A committer's fresh leaf must still be theirs: the update path is
        // where a member could swap in a credential claiming someone else.
        if let Some(leaf) = staged.update_path_leaf_node()
            && leaf_node_ipk(leaf, strict) != Some(author)
        {
            return Err("the committer's new leaf is not bound to them");
        }
        let mut needs_founder = false;
        for p in staged.queued_proposals() {
            match p.proposal() {
                Proposal::Add(a) => {
                    // Whoever is added must arrive as somebody, or the roster
                    // holds a leaf every device reads as a different person.
                    if leaf_node_ipk(a.key_package().leaf_node(), strict).is_none() {
                        return Err("commit adds a leaf bound to no identity");
                    }
                    needs_founder = true;
                },
                Proposal::Remove(r) => {
                    let leaving = matches!(p.sender(), Sender::Member(i) if *i == r.removed());
                    needs_founder |= !leaving;
                },
                // Nothing in promtuz proposes an update — a member refreshes
                // their own leaf by committing — so one is someone else's client.
                _ => return Err("commit carries a proposal kind no member may make"),
            }
        }
        if !needs_founder {
            return Ok(());
        }
        match self.group_meta() {
            Some(meta) if meta.founder == author => Ok(()),
            Some(_) => Err("only the founder may change the membership"),
            None => Err("a pair group's membership never changes"),
        }
    }

    /// Merge a peer's commit if the group's rules allow it: the roster stays
    /// within [`super::MAX_GROUP_MEMBERS`] and [`Self::commit_is_permitted`]
    /// holds for `author`. `Ok(false)` is a refusal — nothing merged, the
    /// epoch unmoved — and is the same answer on every honest device.
    pub fn merge_staged_commit_if_permitted(
        &mut self, provider: &PromtuzMlsProvider, staged: StagedCommit, author: [u8; 32],
    ) -> Result<bool> {
        let roster = self.member_count() + staged.add_proposals().count();
        let why = if roster > super::MAX_GROUP_MEMBERS {
            Some("commit would take the group past its member limit")
        } else {
            self.commit_is_permitted(&staged, author).err()
        };
        if let Some(why) = why {
            log::warn!(
                "GROUP: refusing commit from {} in {}: {why}",
                hex::encode(&author[..4]),
                hex::encode(&self.group_id()[..4])
            );
            return Ok(false);
        }
        self.merge_staged_commit(provider, staged)?;
        Ok(true)
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

    /// Keep a member's proposal until a commit picks it up — a leave, in
    /// practice, which the leaver proposes and someone else carries.
    pub fn store_pending_proposal(
        &mut self, provider: &PromtuzMlsProvider, proposal: QueuedProposal,
    ) -> Result<()> {
        self.inner
            .store_pending_proposal(provider.storage(), proposal)
            .map_err(|e| MlsGroupError::Internal(format!("store proposal: {e:?}")))
    }

    /// Commit whatever proposals are pending. Returns the commit.
    pub fn commit_to_pending_proposals<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S,
    ) -> Result<MlsMessageOut> {
        let (commit, _welcome, _group_info) = self
            .inner
            .commit_to_pending_proposals(provider, signer)
            .map_err(MlsGroupError::from_openmls)?;
        Ok(commit)
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

    /// Whether a commit we built is still waiting to be merged. Set by
    /// [`Self::add_members`] and the other commit builders; cleared by
    /// [`Self::merge_pending_commit`], [`Self::clear_pending_commit`], or by
    /// merging someone else's commit, which openmls treats as superseding ours.
    pub fn has_pending_commit(&self) -> bool {
        self.inner.pending_commit().is_some()
    }

    /// Drop a commit we built but will not merge: the change that made it
    /// died before it reached anyone, so the group stays at the epoch the
    /// members still share. openmls refuses every further membership change
    /// while one is pending and persists that state across restarts, so a
    /// failed add would otherwise be the last change this group ever took.
    /// Whether a pending commit reached anyone is not this layer's to know:
    /// `groups::recover` decides between this and a merge.
    pub fn clear_pending_commit(&mut self, provider: &PromtuzMlsProvider) -> Result<()> {
        self.inner.clear_pending_commit(provider.storage()).map_err(MlsGroupError::Storage)
    }

    /// Current group epoch as a plain `u64`.
    pub fn epoch(&self) -> u64 {
        self.inner.epoch().as_u64()
    }

    /// Current group ID as a 32-byte array.
    ///
    /// Returns the first 32 bytes of the underlying `GroupId` (the
    /// rest is dropped). Group IDs are fixed at 32 B; this defensive
    /// truncation handles loaded-from-disk groups that may have
    /// shorter values — they get zero-padded rather than panicking.
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

    /// What the founder said this group is, or `None` for a 1:1.
    ///
    /// Read from the group context, so it is the same answer on every member's
    /// device and arrives with the Welcome rather than after it. A malformed
    /// blob reads as `None` — a group we cannot describe is safer treated as a
    /// pair than trusted from half-decoded bytes.
    pub fn group_meta(&self) -> Option<GroupMeta> {
        // `extensions()`, not `export_group_context()` — the latter is gated
        // behind openmls's `test-utils`, so it compiles under `cargo test` and
        // vanishes in the build that ships.
        self.inner.extensions().iter().find_map(|e| match e {
            Extension::Unknown(PROMTUZ_GROUP_META_EXT, UnknownExtension(bytes)) => {
                postcard::from_bytes::<GroupMeta>(bytes.as_slice()).ok()
            },
            _ => None,
        })
    }

    /// Iterate members. Returned items expose `index: LeafNodeIndex`,
    /// `credential` and `signature_key`; [`Self::member_ipk`] is how a member
    /// becomes a person.
    pub fn members(&self) -> impl Iterator<Item = Member> + '_ {
        self.inner.members()
    }

    /// Whether this is a group chat (founded with a [`GroupMeta`]) rather
    /// than a pair — the line along which the credential rule tightens.
    pub fn is_group_chat(&self) -> bool {
        self.group_meta().is_some()
    }

    /// The identity a member's leaf is bound to, under this group's rule;
    /// `None` for a leaf that proves nothing.
    pub fn member_ipk(&self, m: &Member) -> Option<[u8; 32]> {
        super::credential::member_ipk(m, self.is_group_chat())
    }

    /// The identity at a leaf, under this group's rule.
    pub fn member_ipk_at(&self, index: LeafNodeIndex) -> Option<[u8; 32]> {
        self.inner.member_at(index).and_then(|m| self.member_ipk(&m))
    }

    /// Everyone whose leaf is bound to an identity, in leaf order.
    pub fn roster(&self) -> Vec<[u8; 32]> {
        let strict = self.is_group_chat();
        self.inner.members().filter_map(|m| super::credential::member_ipk(&m, strict)).collect()
    }

    /// Find a member by their IPK. Returns the leaf index, or `None` if no
    /// member's leaf is bound to it.
    pub fn member_index_by_ipk(&self, ipk: &[u8; 32]) -> Option<LeafNodeIndex> {
        let strict = self.is_group_chat();
        self.inner
            .members()
            .find(|m| super::credential::member_ipk(m, strict) == Some(*ipk))
            .map(|m| m.index)
    }

    /// Export an MLS exporter secret for SFrame / call key derivation.
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
/// `MlsApplicationEnvelopeP::mls_message`. We never invent our own
/// framing for the inner MLS bytes — openmls owns it.
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
    use crate::mls::MLS_PADDING_SIZE;
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
        ipk_signer: ed25519_dalek::SigningKey,
        sig_kp: openmls_basic_credential::SignatureKeyPair,
    }

    impl Party {
        fn new(provider: &PromtuzMlsProvider, ipk_seed: u8) -> Self {
            // IPK is deterministic; leaf signing key is random — the
            // separation mirrors the leaf-key-distinct-from-IPK design.
            let ipk_signer = ed25519_dalek::SigningKey::from_bytes(&[ipk_seed; 32]);
            let ipk = ipk_signer.verifying_key().to_bytes();
            let sig_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
                .expect("sig kp");
            sig_kp.store(provider.storage()).expect("store sig kp");
            Self { ipk, ipk_signer, sig_kp }
        }

        /// The leaf key under a credential its identity signed for.
        fn cwk(&self) -> CredentialWithKey {
            CredentialWithKey {
                credential:    crate::mls::credential::bound_credential(&self.ipk_signer, self.sig_kp.public()).into(),
                signature_key: self.sig_kp.public().into(),
            }
        }
    }

    /// Build a fresh KeyPackage for `party` and persist its bundle in
    /// `provider`'s storage. The KeyPackage itself ships across to a
    /// counterparty's group; the bundle (init+enc keys) stays local.
    fn make_kp(provider: &PromtuzMlsProvider, party: &Party) -> KeyPackage {
        let cwk = party.cwk();
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(Capabilities::new(
                None,
                Some(&[PROMTUZ_CIPHERSUITE]),
                // What the real stash declares, so a leaf can join a group
                // that names its founder in the context.
                Some(&[GROUP_META_EXTENSION]),
                None,
                None,
            ))
            .build(PROMTUZ_CIPHERSUITE, provider, &party.sig_kp, cwk)
            .expect("build kp");
        bundle.key_package().clone()
    }

    /// Helper: Alice creates a 1-member group.
    fn create_group(provider: &PromtuzMlsProvider, party: &Party, gid: &[u8; 32]) -> MlsGroupHandle {
        MlsGroupHandle::create(provider, &party.sig_kp, party.cwk(), gid,
            None,
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
        // Folded from the former add_member_yields_commit_and_welcome:
        // absolute epoch + member-count pins after one add+merge.
        assert_eq!(alice_group.epoch(), 1);
        assert_eq!(alice_group.member_count(), 2);

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
        // Folded from application_round_trip_through_tls_codec: app
        // messages frame as PrivateMessage (cleartext-framing guard).
        assert_eq!(on_bob.wire_format(), WireFormat::PrivateMessage);
        let proto = on_bob.try_into_protocol_message().expect("proto");
        let content = bob_group.process_incoming(&provider_b, proto).expect("process").content;
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
            .expect("alice process")
            .content;
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

    /// Three members, one founder. A commit that evicts someone is the
    /// founder's alone to make; a removal the leaver proposed themselves is a
    /// leave, which anyone may commit.
    #[test]
    fn receivers_refuse_membership_commits_from_anyone_but_the_founder() {
        let (pa, pb, pc) = (build_provider(), build_provider(), build_provider());
        let alice = Party::new(&pa, 1);
        let bob = Party::new(&pb, 2);
        let carol = Party::new(&pc, 3);
        let meta = GroupMeta { title: "room".into(), founder: alice.ipk };
        let mut ga = MlsGroupHandle::create(&pa, &alice.sig_kp, alice.cwk(), &[0xAB; 32], Some(&meta),
        )
        .expect("create");
        let (_c, welcome) = ga
            .add_members(&pa, &alice.sig_kp, &[make_kp(&pb, &bob), make_kp(&pc, &carol)])
            .expect("add");
        ga.merge_pending_commit(&pa).expect("merge");
        let join = |provider: &PromtuzMlsProvider| {
            let w = extract_welcome_via_tls(welcome.clone());
            let staged =
                StagedWelcome::new_from_welcome(provider, &MlsGroupJoinConfig::default(), w, None)
                    .expect("staged");
            MlsGroupHandle::wrap(staged.into_group(provider).expect("into"))
        };
        let mut gb = join(&pb);
        let mut gc = join(&pc);
        let inbound = |g: &mut MlsGroupHandle, provider: &PromtuzMlsProvider, msg: &MlsMessageOut| {
            let in_msg = mls_message_from_bytes(&mls_message_to_bytes(msg).unwrap()).unwrap();
            g.process_incoming(provider, in_msg.try_into_protocol_message().unwrap())
                .expect("process")
                .content
        };
        let commit_of = |c: ProcessedMessageContent| match c {
            ProcessedMessageContent::StagedCommitMessage(s) => *s,
            other => panic!("expected a commit, got {other:?}"),
        };
        let proposal_of = |c: ProcessedMessageContent| match c {
            ProcessedMessageContent::ProposalMessage(p) => *p,
            other => panic!("expected a proposal, got {other:?}"),
        };

        // Bob proposes his own removal; Carol, no founder, commits it; Alice
        // judges Carol's commit: a leave may be carried by anyone.
        let leave = gb.leave(&pb, &bob.sig_kp).expect("leave");
        let p = proposal_of(inbound(&mut ga, &pa, &leave));
        ga.store_pending_proposal(&pa, p).expect("store");
        let p = proposal_of(inbound(&mut gc, &pc, &leave));
        gc.store_pending_proposal(&pc, p).expect("store");
        let carried = gc.commit_to_pending_proposals(&pc, &carol.sig_kp).expect("commit");
        let s = commit_of(inbound(&mut ga, &pa, &carried));
        assert!(ga.commit_is_permitted(&s, carol.ipk).is_ok(), "a leave may be carried by anyone");
        ga.merge_staged_commit(&pa, s).expect("merge");
        gc.merge_pending_commit(&pc).expect("merge");
        assert_eq!(ga.member_count(), 2);

        // Carol evicts Alice: refused — only the founder removes anyone
        // who did not ask to go.
        let alice_idx = gc.member_index_by_ipk(&alice.ipk).expect("alice");
        let evict = gc.remove_members(&pc, &carol.sig_kp, &[alice_idx]).expect("commit");
        let s = commit_of(inbound(&mut ga, &pa, &evict));
        assert!(ga.commit_is_permitted(&s, carol.ipk).is_err(), "carol may not evict the founder");
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

    #[test]
    fn short_messages_of_different_lengths_seal_to_the_same_size() {
        let provider = build_provider();
        let alice = Party::new(&provider, 1);
        let mut group = create_group(&provider, &alice, &[0xAA; 32]);

        let seal = |g: &mut MlsGroupHandle, body: &[u8]| {
            let msg = g
                .create_application_message(&provider, &alice.sig_kp, body)
                .expect("encrypt");
            mls_message_to_bytes(&msg).expect("ser").len()
        };

        assert!(MLS_PADDING_SIZE >= 64);
        assert_eq!(seal(&mut group, b"ok"), seal(&mut group, &vec![b'x'; 60]));
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
        let bytes = mls_message_to_bytes(&proposal).expect("ser");
        assert_eq!(
            mls_message_from_bytes(&bytes).expect("deser").wire_format(),
            WireFormat::PrivateMessage
        );
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

    // -------------------------------------------------------------
    // Tests 11-12: dropping one group's state, over real openmls state.
    // -------------------------------------------------------------

    /// A provider plus the connection under it, so a test can count the rows
    /// each group actually occupies.
    fn provider_with_conn() -> (PromtuzMlsProvider, Arc<Mutex<Connection>>) {
        let mut raw = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut raw);
        let conn = Arc::new(Mutex::new(raw));
        (PromtuzMlsProvider::new(Arc::clone(&conn)), conn)
    }

    /// Distinct groups holding `mls_storage` rows, and rows in the size
    /// sidecar. The `group_id` column is the CBOR-encoded `GroupId` openmls
    /// hands the provider, never the raw 32 bytes, so the tally is by count
    /// and by what still loads rather than by matching an id here.
    fn tally(conn: &Arc<Mutex<Connection>>) -> (i64, i64) {
        let conn = conn.lock();
        let groups = conn
            .query_row(
                "SELECT COUNT(DISTINCT group_id) FROM mls_storage WHERE length(group_id) > 0",
                [],
                |r| r.get(0),
            )
            .expect("count groups");
        let sidecar = conn
            .query_row("SELECT COUNT(*) FROM mls_group_size", [], |r| r.get(0))
            .expect("count sidecar");
        (groups, sidecar)
    }

    /// What deleting a group conversation must leave of that group: nothing.
    /// `delete_conversation`'s `purge_mls_group` runs exactly these two steps
    /// and needs both — openmls's own `delete` keeps no account of the size
    /// sidecar, so the row is still standing when it returns.
    ///
    /// This is as close as a unit test gets: `delete_conversation` resolves
    /// `Identity::get()` and `PromtuzMlsProvider::shared()`, both real files,
    /// so *that* it purges is not covered here — only that purging is total.
    #[test]
    fn purging_a_group_leaves_no_storage_rows_for_it() {
        let (provider, conn) = provider_with_conn();
        let alice = Party::new(&provider, 1);
        let live = [0x11; 32];
        let doomed = [0x22; 32];
        create_group(&provider, &alice, &live);
        let mut group = create_group(&provider, &alice, &doomed);
        assert_eq!(tally(&conn), (2, 2));

        group.delete(&provider).expect("openmls delete");
        assert_eq!(tally(&conn), (1, 2), "openmls's delete is not the whole job");

        provider.storage().forget_group(&doomed).expect("forget");

        assert_eq!(tally(&conn), (1, 1));
        assert!(MlsGroupHandle::load(&provider, &doomed).expect("load").is_none());
        assert!(MlsGroupHandle::load(&provider, &live).expect("load").is_some());
    }

    /// Removal takes the whole group and nothing else — including the
    /// `mls_group_size` sidecar, which is kept by deltas and so survives the
    /// rows it counted.
    #[test]
    fn forget_group_takes_the_sidecar_and_leaves_the_neighbour() {
        let (provider, conn) = provider_with_conn();
        let alice = Party::new(&provider, 1);
        let live = [0x11; 32];
        let orphan = [0x22; 32];
        create_group(&provider, &alice, &live);
        create_group(&provider, &alice, &orphan);
        assert_eq!(tally(&conn), (2, 2));

        provider.storage().forget_group(&orphan).expect("forget");

        assert_eq!(tally(&conn), (1, 1));
        assert!(MlsGroupHandle::load(&provider, &live).expect("load").is_some());
        assert!(MlsGroupHandle::load(&provider, &orphan).expect("load").is_none());
    }

    /// The leftover survives a reload, since openmls persists it, so a failure
    /// path has to drop it explicitly rather than let it fall out of scope.
    #[test]
    fn an_unmerged_commit_can_be_dropped_and_stays_dropped_after_reload() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let gid = [0xAC; 32];
        let mut group = create_group(&provider_a, &alice, &gid);
        group.add_members(&provider_a, &alice.sig_kp, &[make_kp(&provider_b, &bob)]).expect("add");

        let mut reloaded = MlsGroupHandle::load(&provider_a, &gid).expect("load").expect("stored");
        assert!(reloaded.has_pending_commit(), "openmls persists the pending commit");

        reloaded.clear_pending_commit(&provider_a).expect("drop");
        assert!(!reloaded.has_pending_commit());
        let again = MlsGroupHandle::load(&provider_a, &gid).expect("load").expect("stored");
        assert!(!again.has_pending_commit(), "the drop is persisted too");
        assert_eq!(again.epoch(), 0);
    }

    /// The other fate of an unmerged commit: one that left the device is
    /// merged as it stands, and the merge lands on exactly the epoch the
    /// Welcome it produced put the joiner on, so founder and joiner agree and
    /// keep talking. Building afresh instead would leave them holding
    /// different keys at the same epoch.
    #[test]
    fn a_recovered_merge_lands_where_the_welcome_put_the_joiner() {
        let provider_a = build_provider();
        let provider_b = build_provider();
        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);
        let gid = [0xAD; 32];
        let mut group = create_group(&provider_a, &alice, &gid);
        let (_commit, welcome) = group
            .add_members(&provider_a, &alice.sig_kp, &[make_kp(&provider_b, &bob)])
            .expect("add");
        // The change dies here: the Welcome is out, the merge never happened.
        drop(group);

        let staged = StagedWelcome::new_from_welcome(
            &provider_b,
            &MlsGroupJoinConfig::default(),
            extract_welcome_via_tls(welcome),
            None,
        )
        .expect("staged");
        let mut bob_group = MlsGroupHandle::wrap(staged.into_group(&provider_b).expect("into"));

        let mut recovered = MlsGroupHandle::load(&provider_a, &gid).expect("load").expect("stored");
        assert!(recovered.has_pending_commit(), "the commit is still waiting");
        recovered.merge_pending_commit(&provider_a).expect("merge the recovered commit");
        assert_eq!(recovered.epoch(), bob_group.epoch(), "founder and joiner agree on the epoch");
        assert_eq!(recovered.roster(), bob_group.roster(), "and on who is in the group");

        // The proof that the keys agree: a message after the recovery decrypts.
        let msg = recovered
            .create_application_message(&provider_a, &alice.sig_kp, b"still here")
            .expect("encrypt");
        let on_bob =
            mls_message_from_bytes(&mls_message_to_bytes(&msg).expect("ser")).expect("deser");
        let proto = on_bob.try_into_protocol_message().expect("proto");
        match bob_group.process_incoming(&provider_b, proto).expect("process").content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), b"still here");
            },
            other => panic!("expected an application message, got {other:?}"),
        }
    }
}
