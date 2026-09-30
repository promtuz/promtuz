//! `MlsGroupHandle` — high-level wrapper over `openmls::group::MlsGroup`
//! exposing the promtuz-flavored group API.
//!
//! # Scope
//!
//! - **Lifecycle**: create / add / remove / self-update / leave.
//! - **Application messaging**: encrypt-out, decrypt-in (inner MLS message wrapping; outer envelope
//!   is in `welcome.rs` and the `messaging.rs` wiring).
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

use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::SignedChange;
use common::proto::mls_wire::group_change_signing_input;
use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;
use openmls_traits::OpenMlsProvider;
use openmls_traits::signatures::Signer;
use serde::Deserialize;
use serde::Serialize;

use super::policy;
use super::policy::GroupState;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMeta {
    /// The group's name as its committer last knew it. Renames travel as
    /// `SystemEvent::Titled`; this is what a new member starts from.
    pub title:   String,
    /// Who founded it.
    ///
    /// Carried here rather than inferred from whoever sent us the Welcome: a
    /// group whose state landed without a conversation is homed by the next
    /// message to arrive in it, and the sender of that message is whoever
    /// happened to speak first. Reading it from the context means every
    /// member agrees on it however they came to learn about the group.
    pub founder: [u8; 32],
    /// Who runs the group and by what rules. `None` for a group founded before
    /// rules were signed, which its founder alone runs until it converts.
    pub state:   Option<GroupState>,
}

/// The part of [`GroupMeta`] every client reads. The state follows it in the
/// same extension, where clients from before it stop reading.
#[derive(Serialize, Deserialize)]
struct MetaHead {
    title:   String,
    #[serde(with = "serde_bytes")]
    founder: [u8; 32],
}

impl GroupMeta {
    pub fn founded(title: String, founder: [u8; 32]) -> Self {
        Self { title, founder, state: Some(GroupState::founded(founder)) }
    }

    /// The rules this group runs by. A group from before they were signed runs
    /// as it always did: its founder alone manages it.
    pub fn effective(&self) -> GroupState {
        self.state.clone().unwrap_or_else(|| {
            let mut state = GroupState::founded(self.founder);
            state.rules.members_add = false;
            state
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let head = MetaHead { title: self.title.clone(), founder: self.founder };
        let mut bytes =
            postcard::to_allocvec(&head).map_err(|e| MlsGroupError::Codec(e.to_string()))?;
        if let Some(state) = &self.state {
            bytes.extend(
                postcard::to_allocvec(state).map_err(|e| MlsGroupError::Codec(e.to_string()))?,
            );
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (head, rest) = postcard::take_from_bytes::<MetaHead>(bytes).ok()?;
        let state = if rest.is_empty() { None } else { Some(postcard::from_bytes(rest).ok()?) };
        Some(Self { title: head.title, founder: head.founder, state })
    }

    /// The group context that carries this meta. Every GroupContextExtensions
    /// proposal has to name its extensions as required, so the group is
    /// founded with them required too.
    fn extensions(&self) -> Result<Extensions<GroupContext>> {
        Extensions::from_vec(vec![
            Extension::Unknown(PROMTUZ_GROUP_META_EXT, UnknownExtension(self.encode()?)),
            Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[GROUP_META_EXTENSION],
                &[],
                &[],
            )),
        ])
        .map_err(|e| MlsGroupError::Codec(format!("group meta extension: {e}")))
    }

    fn from_extensions(exts: &Extensions<GroupContext>) -> Option<Self> {
        exts.iter().find_map(|e| match e {
            Extension::Unknown(PROMTUZ_GROUP_META_EXT, UnknownExtension(bytes)) => {
                Self::decode(bytes)
            },
            _ => None,
        })
    }
}

/// A change a merged commit made, and the rules it replaced.
#[derive(Debug, Clone)]
pub struct Changed {
    pub signed: SignedChange,
    pub before: GroupState,
}

/// What became of a peer's commit.
pub enum CommitOutcome {
    /// Refused on every honest device alike: nothing merged.
    Refused,
    /// Merged. For a group with signed rules, the change it made.
    Merged(Option<Changed>),
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
            let exts = meta.extensions()?;
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
        let loaded = MlsGroup::load(provider.storage(), &gid).map_err(MlsGroupError::Storage)?;
        let Some(mut inner) = loaded else { return Ok(None) };
        // Older joiners stored a configuration that omitted trees from their
        // own later Welcomes. Normalize our local transport configuration too,
        // so an existing member can become committer and invite someone.
        let config = MlsGroupJoinConfig::builder()
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .padding_size(super::MLS_PADDING_SIZE)
            .use_ratchet_tree_extension(true)
            .sender_ratchet_configuration(
                inner.configuration().sender_ratchet_configuration().clone(),
            )
            .build();
        if inner.configuration() != &config {
            inner.set_configuration(provider.storage(), &config).map_err(MlsGroupError::Storage)?;
        }
        Ok(Some(Self { inner }))
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
    /// - For `StagedCommitMessage`: call [`Self::merge_staged_commit`] to advance the local epoch.
    /// - For `ProposalMessage`: queue via openmls's `store_pending_proposal`.
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

    /// The group's rules, applied on receipt. MLS lets any member commit any
    /// proposal, and a rule only the sending client keeps is only as good as
    /// that client, so every receiver checks every commit here and they all
    /// reach the same answer.
    ///
    /// With signed rules, only the committer named in the group's state
    /// commits, apart from an admin taking its place. Every change arrives
    /// signed by whoever asked for it, is checked against their role by
    /// [`policy::apply`], and must be exactly what the commit's proposals do.
    /// A commit with no change may only refresh the committer's keys.
    ///
    /// A group from before signed rules keeps its old rule: only the founder
    /// adds, or removes anyone who didn't propose their own removal. Its one
    /// other allowed change is the founder converting it. A pair group has no
    /// founder and its roster never changes.
    ///
    /// Returns the verified change, if the commit made one.
    pub fn commit_is_permitted(
        &self, staged: &StagedCommit, author: [u8; 32],
    ) -> std::result::Result<Option<SignedChange>, &'static str> {
        use super::credential::leaf_node_ipk;
        let strict = self.is_group_chat();
        // A committer's fresh leaf must still be theirs: the update path is
        // where a member could swap in a credential claiming someone else.
        if let Some(leaf) = staged.update_path_leaf_node()
            && leaf_node_ipk(leaf, strict) != Some(author)
        {
            return Err("the committer's new leaf is not bound to them");
        }
        let mut adds = Vec::new();
        // Who each removal takes out, and whether they proposed it themselves.
        let mut removes = Vec::new();
        let mut next = None;
        for p in staged.queued_proposals() {
            match p.proposal() {
                // Whoever is added must arrive as somebody, or the roster
                // holds a leaf every device reads as a different person.
                Proposal::Add(a) => adds.push(
                    leaf_node_ipk(a.key_package().leaf_node(), strict)
                        .ok_or("commit adds a leaf bound to no identity")?,
                ),
                Proposal::Remove(r) => removes.push((
                    self.member_ipk_at(r.removed())
                        .ok_or("commit removes a leaf bound to no one")?,
                    matches!(p.sender(), Sender::Member(i) if *i == r.removed()),
                )),
                Proposal::GroupContextExtensions(e) => {
                    let only_ours = e.extensions().iter().all(|x| {
                        matches!(
                            x,
                            Extension::Unknown(PROMTUZ_GROUP_META_EXT, _)
                                | Extension::RequiredCapabilities(_)
                        )
                    });
                    next = Some(
                        GroupMeta::from_extensions(e.extensions())
                            .filter(|_| only_ours)
                            .ok_or("commit rewrites the group into something unreadable")?,
                    );
                },
                // Members never propose key updates or anything else; only the
                // committer refreshes its leaf, through its commit's path.
                _ => return Err("commit carries a proposal kind no member may make"),
            }
        }
        let Some(meta) = self.group_meta() else {
            if self.is_group_chat() {
                return Err("group rules are not supported");
            }
            return if adds.is_empty() && removes.is_empty() && next.is_none() {
                Ok(None)
            } else {
                Err("a pair group's membership never changes")
            };
        };
        let Some(next) = next else {
            let Some(state) = &meta.state else {
                let leaves_only = adds.is_empty() && removes.iter().all(|(_, leaving)| *leaving);
                return if leaves_only || author == meta.founder {
                    Ok(None)
                } else {
                    Err("only the founder may change the membership")
                };
            };
            return if !adds.is_empty() || !removes.is_empty() {
                Err("a membership change must say who asked for it")
            } else if author != state.committer {
                Err("only the committer changes the group")
            } else {
                Ok(None)
            };
        };
        if next.founder != meta.founder {
            return Err("commit renames the group's founder");
        }
        let state_after = next.state.as_ref().ok_or("commit drops the group's rules")?;
        let signed = state_after.last.clone().ok_or("commit carries no change")?;
        let expected = self.state_after_change(&author, &signed)?;
        if *state_after != expected {
            return Err("the group's new rules aren't what the change makes them");
        }
        let proposals_match = match &signed.change {
            GroupChange::Add { who } => {
                let mut asked: Vec<_> = who.iter().map(|w| w.0).collect();
                asked.sort();
                asked.dedup();
                adds.sort();
                removes.is_empty() && asked.len() == who.len() && asked == adds
            },
            GroupChange::Remove { who } => {
                adds.is_empty() && removes.len() == 1 && removes[0].0 == who.0
            },
            GroupChange::Leave { .. } => {
                adds.is_empty() && removes.len() == 1 && removes[0].0 == signed.by.0
            },
            GroupChange::MemberRequest(request) => {
                (if request.action == common::proto::mls_wire::GroupMemberAction::Refresh {
                    adds == [request.who.0]
                } else {
                    adds.is_empty()
                }) && removes.len() == 1
                    && removes[0].0 == request.who.0
            },
            _ => adds.is_empty() && removes.is_empty(),
        };
        if !proposals_match {
            return Err("the commit doesn't do what its change says");
        }
        Ok(Some(signed))
    }

    /// The same signature and permission checks guard both commit creation and
    /// receipt. In particular, an authenticated transport is not a signature on
    /// the request the committer will carry for another member.
    pub fn state_after_change(
        &self, author: &[u8; 32], signed: &SignedChange,
    ) -> std::result::Result<GroupState, &'static str> {
        self.verify_change(signed)?;
        if let GroupChange::MemberRequest(request) = &signed.change {
            super::branch_proof::verify_member_request(&self.group_id(), request)
                .map_err(|_| "invalid member resync authorization")?;
        }
        let meta = self.group_meta().ok_or("not a group chat")?;
        match &meta.state {
            Some(state) => policy::apply(state, &self.roster(), author, signed),
            None if signed.change == GroupChange::Upgrade
                && signed.by.0 == meta.founder
                && *author == meta.founder =>
            {
                Ok(GroupState { last: Some(signed.clone()), ..GroupState::founded(meta.founder) })
            },
            None => Err("only the founder converts the group"),
        }
    }

    /// Authorize application intent against the epoch that decrypted it. A
    /// catch-up drain can cross several role/rule changes before persistence.
    pub(crate) fn application_is_permitted(&self, author: &[u8; 32], plaintext: &[u8]) -> bool {
        use common::proto::mls_wire::AppPayload;
        use common::proto::mls_wire::SystemEvent;
        use common::proto::pack::Unpacker;
        let Ok(payload) = AppPayload::deser(plaintext) else { return true };
        let Some(meta) = self.group_meta() else {
            if self.is_group_chat() {
                return false;
            }
            return !matches!(
                payload,
                AppPayload::System(_)
                    | AppPayload::GroupPicture { .. }
                    | AppPayload::GroupRequest(_)
                    | AppPayload::GroupWelcome { .. }
                    | AppPayload::GroupInvitation { .. }
                    | AppPayload::GroupAdmins { .. }
            );
        };
        if !self.roster().contains(author) {
            return false;
        }
        let state = meta.effective();
        match payload {
            AppPayload::Post { .. }
            | AppPayload::Text(_)
            | AppPayload::Reply { .. }
            | AppPayload::Image { .. }
            | AppPayload::Attachment { .. }
            | AppPayload::Edit { .. }
            | AppPayload::Revise { .. } => state.may_send(author),
            AppPayload::GroupPicture { .. } | AppPayload::System(SystemEvent::Titled { .. }) => {
                state.may_edit(author)
            },
            AppPayload::System(SystemEvent::Added { .. } | SystemEvent::Removed { .. }) => {
                meta.state.is_none() && *author == meta.founder
            },
            AppPayload::System(SystemEvent::Left { who }) => {
                meta.state.is_none() && who.0 == *author
            },
            AppPayload::GroupWelcome { .. } | AppPayload::GroupInvitation { .. } => {
                state.role(author) >= policy::ROLE_ADMIN
            },
            AppPayload::System(_) | AppPayload::GroupAdmins { .. } => false,
            _ => true,
        }
    }

    /// Signed by who it says, for this group at this epoch.
    pub(crate) fn verify_change(
        &self, signed: &SignedChange,
    ) -> std::result::Result<(), &'static str> {
        if signed.epoch != self.epoch() {
            return Err("the change was signed for another epoch");
        }
        if signed.branch.0 != self.branch_id() {
            return Err("the change was signed for another branch");
        }
        let input = group_change_signing_input(
            &self.group_id(),
            signed.epoch,
            &signed.branch.0,
            &signed.by.0,
            &signed.change,
        );
        ed25519_dalek::VerifyingKey::from_bytes(&signed.by.0)
            .and_then(|k| {
                k.verify_strict(&input, &ed25519_dalek::Signature::from_bytes(&signed.sig.0))
            })
            .map_err(|_| "the change's signature doesn't hold")
    }

    /// Merge a peer's commit if the group's rules allow it: the roster stays
    /// within [`super::MAX_GROUP_MEMBERS`] and [`Self::commit_is_permitted`]
    /// holds for `author`. A refusal merges nothing and leaves the epoch where
    /// it was, and is the same answer on every honest device.
    pub fn merge_staged_commit_if_permitted(
        &mut self, provider: &PromtuzMlsProvider, staged: StagedCommit, author: [u8; 32],
    ) -> Result<CommitOutcome> {
        let roster = self.member_count() + staged.add_proposals().count();
        let verdict = if roster > super::MAX_GROUP_MEMBERS {
            Err("commit would take the group past its member limit")
        } else {
            self.commit_is_permitted(&staged, author)
        };
        match verdict {
            Ok(change) => {
                let before = self.group_meta().map(|m| m.effective());
                self.merge_staged_commit(provider, staged)?;
                Ok(CommitOutcome::Merged(
                    change.zip(before).map(|(signed, before)| Changed { signed, before }),
                ))
            },
            Err(why) => {
                log::warn!(
                    "GROUP: refusing commit from {} in {}: {why}",
                    hex::encode(&author[..4]),
                    hex::encode(&self.group_id()[..4])
                );
                Ok(CommitOutcome::Refused)
            },
        }
    }

    /// Commit `meta` as the group's new context, with the adds and removals
    /// its change makes, as one commit. Returns the commit and, for adds, the
    /// Welcome. **The caller merges the pending commit afterwards.**
    pub fn commit_meta<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, meta: &GroupMeta,
        adds: Vec<KeyPackage>, removes: Vec<LeafNodeIndex>,
    ) -> Result<(MlsMessageOut, Option<MlsMessageOut>)> {
        let bundle = self
            .inner
            .commit_builder()
            .consume_proposal_store(false)
            .propose_adds(adds)
            .propose_removals(removes)
            .propose_group_context_extensions(meta.extensions()?)
            .map_err(MlsGroupError::from_openmls)?
            .load_psks(provider.storage())
            .map_err(MlsGroupError::from_openmls)?
            .build(provider.rand(), provider.crypto(), signer, |_| true)
            .map_err(MlsGroupError::from_openmls)?
            .stage_commit(provider)
            .map_err(MlsGroupError::from_openmls)?;
        let (commit, welcome, _group_info) = bundle.into_messages();
        Ok((commit, welcome))
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

    /// Drop a commit we built but won't send, so the group takes the next one.
    pub fn clear_pending_commit(&mut self, provider: &PromtuzMlsProvider) {
        if let Err(e) = self.inner.clear_pending_commit(provider.storage()) {
            log::warn!("GROUP: could not drop an unsent commit: {e:?}");
        }
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

    /// Public, domain-separated identity of an MLS epoch. Epoch numbers alone
    /// cannot distinguish two valid commits made from the same parent.
    pub fn branch_id(&self) -> [u8; 32] {
        use sha2::Digest;
        use sha2::Sha256;
        let mut hash = Sha256::new();
        hash.update(b"promtuz group branch v1");
        hash.update(self.group_id());
        hash.update(self.epoch().to_be_bytes());
        // OpenMLS exits before updating the transcript/confirmation tag when
        // our leaf is removed. The new public tree and extensions are already
        // installed on every recipient, including that removed member.
        hash.update(
            self.inner
                .export_ratchet_tree()
                .tls_serialize_detached()
                .expect("serializable ratchet tree"),
        );
        hash.update(
            self.inner
                .extensions()
                .tls_serialize_detached()
                .expect("serializable group extensions"),
        );
        hash.finalize().into()
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
        GroupMeta::from_extensions(self.inner.extensions())
    }

    /// Iterate members. Returned items expose `index: LeafNodeIndex`,
    /// `credential` and `signature_key`; [`Self::member_ipk`] is how a member
    /// becomes a person.
    pub fn members(&self) -> impl Iterator<Item = Member> + '_ {
        self.inner.members()
    }

    /// Only migration uses the legacy identity at our own authenticated seat.
    pub(crate) fn migration_identity(&self) -> Option<[u8; 32]> {
        self.inner.member_at(self.inner.own_leaf_index())
            .and_then(|m| super::credential::member_ipk(&m, false))
    }

    /// Whether this is a group chat (founded with a [`GroupMeta`]) rather
    /// than a pair — the line along which the credential rule tightens.
    pub fn is_group_chat(&self) -> bool {
        self.inner
            .extensions()
            .iter()
            .any(|e| matches!(e, Extension::Unknown(PROMTUZ_GROUP_META_EXT, _)))
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
    include!("recovery_tests.rs");
    include!("migration_tests.rs");
    use std::sync::Arc;

    use openmls::prelude::tls_codec::Deserialize as _;
    use parking_lot::Mutex;
    use rusqlite::Connection;

    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use crate::mls::MLS_PADDING_SIZE;

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
                credential:    crate::mls::credential::bound_credential(
                    &self.ipk_signer,
                    self.sig_kp.public(),
                )
                .into(),
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
    fn create_group(
        provider: &PromtuzMlsProvider, party: &Party, gid: &[u8; 32],
    ) -> MlsGroupHandle {
        MlsGroupHandle::create(provider, &party.sig_kp, party.cwk(), gid, None)
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
        let staged = StagedWelcome::new_from_welcome(&provider_b, &join_config, welcome_msg, None)
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
            },
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
            .process_incoming(&provider_a, on_alice.try_into_protocol_message().expect("proto"))
            .expect("alice process")
            .content;
        match content {
            ProcessedMessageContent::ApplicationMessage(app) => {
                assert_eq!(app.into_bytes(), plaintext_b);
            },
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
        let staged = StagedWelcome::new_from_welcome(&provider_b, &join_config, welcome_msg, None)
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

    use common::proto::mls_wire::GroupRules;

    fn joined(provider: &PromtuzMlsProvider, welcome: &MlsMessageOut) -> MlsGroupHandle {
        let w = extract_welcome_via_tls(welcome.clone());
        let staged =
            StagedWelcome::new_from_welcome(provider, &MlsGroupJoinConfig::default(), w, None)
                .expect("staged");
        MlsGroupHandle::wrap(staged.into_group(provider).expect("into"))
    }

    fn inbound(
        g: &mut MlsGroupHandle, provider: &PromtuzMlsProvider, msg: &MlsMessageOut,
    ) -> ProcessedMessageContent {
        let in_msg = mls_message_from_bytes(&mls_message_to_bytes(msg).unwrap()).unwrap();
        g.process_incoming(provider, in_msg.try_into_protocol_message().unwrap())
            .expect("process")
            .content
    }

    fn commit_of(c: ProcessedMessageContent) -> StagedCommit {
        match c {
            ProcessedMessageContent::StagedCommitMessage(s) => *s,
            other => panic!("expected a commit, got {other:?}"),
        }
    }

    /// What `g` makes of `commit` from `author`.
    fn judge(
        g: &mut MlsGroupHandle, provider: &PromtuzMlsProvider, commit: &MlsMessageOut,
        author: [u8; 32],
    ) -> std::result::Result<Option<SignedChange>, &'static str> {
        let staged = commit_of(inbound(g, provider, commit));
        g.commit_is_permitted(&staged, author)
    }

    fn signed_by(g: &MlsGroupHandle, p: &Party, change: GroupChange) -> SignedChange {
        use ed25519_dalek::Signer as _;
        let sig = p.ipk_signer.sign(&group_change_signing_input(
            &g.group_id(),
            g.epoch(),
            &g.branch_id(),
            &p.ipk,
            &change,
        ));
        SignedChange {
            by: p.ipk.into(),
            epoch: g.epoch(),
            branch: g.branch_id().into(),
            change,
            sig: common::types::bytes::Bytes(sig.to_bytes()),
        }
    }

    /// `g`'s meta with `state` after `signed`, as its commit would carry it.
    fn meta_after(g: &MlsGroupHandle, state: GroupState, signed: SignedChange) -> GroupMeta {
        GroupMeta {
            state: Some(GroupState { last: Some(signed), ..state }),
            ..g.group_meta().unwrap()
        }
    }

    /// A group from before signed rules: a commit that evicts someone is the
    /// founder's alone to make, a removal the leaver proposed themselves is a
    /// leave, which anyone may commit, and only the founder converts it.
    #[test]
    fn a_group_from_before_signed_rules_keeps_its_founder_rule() {
        let (pa, pb, pc) = (build_provider(), build_provider(), build_provider());
        let alice = Party::new(&pa, 1);
        let bob = Party::new(&pb, 2);
        let carol = Party::new(&pc, 3);
        let meta = GroupMeta { title: "room".into(), founder: alice.ipk, state: None };
        let mut ga =
            MlsGroupHandle::create(&pa, &alice.sig_kp, alice.cwk(), &[0xAB; 32], Some(&meta))
                .expect("create");
        let (_c, welcome) = ga
            .add_members(&pa, &alice.sig_kp, &[make_kp(&pb, &bob), make_kp(&pc, &carol)])
            .expect("add");
        ga.merge_pending_commit(&pa).expect("merge");
        let mut gb = joined(&pb, &welcome);
        let mut gc = joined(&pc, &welcome);
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
        assert!(judge(&mut ga, &pa, &evict, carol.ipk).is_err(), "carol may not evict the founder");
        gc.clear_pending_commit(&pc);

        // Only the founder converts the group to signed rules.
        let upgrade = |g: &MlsGroupHandle, p: &Party| {
            let signed = signed_by(g, p, GroupChange::Upgrade);
            meta_after(g, GroupState::founded(alice.ipk), signed)
        };
        let (forged, _) = gc
            .commit_meta(&pc, &carol.sig_kp, &upgrade(&gc, &carol), vec![], vec![])
            .expect("commit");
        gc.clear_pending_commit(&pc);
        assert!(
            judge(&mut ga, &pa, &forged, carol.ipk).is_err(),
            "carol may not convert alice's group"
        );
        let (converted, _) = ga
            .commit_meta(&pa, &alice.sig_kp, &upgrade(&ga, &alice), vec![], vec![])
            .expect("commit");
        let change = judge(&mut gc, &pc, &converted, alice.ipk).expect("the founder converts it");
        assert_eq!(change.map(|c| c.change), Some(GroupChange::Upgrade));
    }

    /// With signed rules, only the committer commits, apart from an admin
    /// taking its place; every change is signed by whoever asked for it and
    /// held to their role; and the commit must do exactly what the change says.
    #[test]
    fn receivers_hold_every_commit_to_the_signed_rules() {
        let (pa, pb, pc, pd) =
            (build_provider(), build_provider(), build_provider(), build_provider());
        let alice = Party::new(&pa, 1);
        let bob = Party::new(&pb, 2);
        let carol = Party::new(&pc, 3);
        let dave = Party::new(&pd, 4);
        let meta = GroupMeta::founded("room".into(), alice.ipk);
        let mut ga =
            MlsGroupHandle::create(&pa, &alice.sig_kp, alice.cwk(), &[0xAC; 32], Some(&meta))
                .expect("create");
        let (_c, welcome) = ga
            .add_members(&pa, &alice.sig_kp, &[make_kp(&pb, &bob), make_kp(&pc, &carol)])
            .expect("add");
        ga.merge_pending_commit(&pa).expect("merge");
        let mut gb = joined(&pb, &welcome);
        let mut gc = joined(&pc, &welcome);
        let state = ga.group_meta().unwrap().state.unwrap();

        // Alice commits what `signed` asks and `state` says, then drops it.
        let mut probe = |ga: &mut MlsGroupHandle,
                         signed: SignedChange,
                         state: GroupState,
                         adds: Vec<KeyPackage>| {
            let (commit, _) = ga
                .commit_meta(&pa, &alice.sig_kp, &meta_after(ga, state, signed), adds, vec![])
                .unwrap();
            ga.clear_pending_commit(&pa);
            judge(&mut gc, &pc, &commit, alice.ipk)
        };
        let add_dave = GroupChange::Add { who: vec![dave.ipk.into()] };
        let asked = signed_by(&ga, &bob, add_dave.clone());
        assert!(
            probe(&mut ga, asked.clone(), state.clone(), vec![make_kp(&pd, &dave)]).is_ok(),
            "members may add"
        );
        assert!(
            probe(&mut ga, asked, state.clone(), vec![]).is_err(),
            "the commit must add whoever was asked for"
        );
        let mut forged = signed_by(&ga, &carol, add_dave.clone());
        forged.by = bob.ipk.into();
        assert!(
            probe(&mut ga, forged, state.clone(), vec![make_kp(&pd, &dave)]).is_err(),
            "bob never signed it"
        );
        let quiet = GroupRules { members_send: false, ..state.rules };
        let asked = signed_by(&ga, &bob, GroupChange::Rules(quiet));
        assert!(
            probe(&mut ga, asked, GroupState { rules: quiet, ..state.clone() }, vec![]).is_err(),
            "members set no rules"
        );
        let asked = signed_by(&ga, &alice, GroupChange::Rules(quiet));
        assert!(
            probe(&mut ga, asked.clone(), state.clone(), vec![]).is_err(),
            "the state must be what the change makes"
        );

        // Carol isn't the committer: not for rules, not for her own keys.
        let signed = signed_by(&gc, &carol, GroupChange::Rules(quiet));
        let (commit, _) = gc
            .commit_meta(
                &pc,
                &carol.sig_kp,
                &meta_after(&gc, GroupState { rules: quiet, ..state.clone() }, signed),
                vec![],
                vec![],
            )
            .unwrap();
        gc.clear_pending_commit(&pc);
        assert!(judge(&mut gb, &pb, &commit, carol.ipk).is_err(), "only the committer commits");
        let update = gc.self_update(&pc, &carol.sig_kp).unwrap();
        gc.clear_pending_commit(&pc);
        assert!(judge(&mut gb, &pb, &update, carol.ipk).is_err(), "not even a key update");

        // Alice makes Bob an admin, and Bob can then take over.
        let promote = signed_by(
            &ga,
            &alice,
            GroupChange::Role { who: bob.ipk.into(), role: policy::ROLE_ADMIN },
        );
        let promoted = GroupState { admins: vec![bob.ipk], ..state.clone() };
        let (commit, _) = ga
            .commit_meta(
                &pa,
                &alice.sig_kp,
                &meta_after(&ga, promoted.clone(), promote),
                vec![],
                vec![],
            )
            .unwrap();
        for (g, p) in [(&mut gb, &pb), (&mut gc, &pc)] {
            let staged = commit_of(inbound(g, p, &commit));
            assert!(matches!(
                g.merge_staged_commit_if_permitted(p, staged, alice.ipk).unwrap(),
                CommitOutcome::Merged(Some(_))
            ));
        }
        ga.merge_pending_commit(&pa).unwrap();
        let takeover = signed_by(&gb, &bob, GroupChange::Takeover);
        let taken = GroupState { committer: bob.ipk, ..promoted };
        let (commit, _) = gb
            .commit_meta(&pb, &bob.sig_kp, &meta_after(&gb, taken, takeover), vec![], vec![])
            .unwrap();
        assert!(judge(&mut gc, &pc, &commit, bob.ipk).is_ok(), "an admin may take over");
    }

    fn recovery_commit(
        provider: &PromtuzMlsProvider, gid: [u8; 32], author: &Party, change: GroupChange,
    ) -> ([u8; 32], Vec<u8>) {
        use crate::mls::recovery::Candidate;
        use crate::mls::recovery::Transaction;
        let mut tx = Transaction::open(provider, gid, None).unwrap().unwrap();
        let parent = tx.parent;
        let signed = signed_by(&tx.group, author, change);
        let before = tx.group.group_meta().unwrap().effective();
        let after = tx.group.state_after_change(&author.ipk, &signed).unwrap();
        let mut meta = tx.group.group_meta().unwrap();
        meta.state = Some(after);
        let (commit, _) =
            tx.group.commit_meta(&tx.provider, &author.sig_kp, &meta, vec![], vec![]).unwrap();
        let bytes = mls_message_to_bytes(&commit).unwrap();
        tx.group.merge_pending_commit(&tx.provider).unwrap();
        tx.publish(
            Some(Candidate {
                rank:    before.role(&author.ipk),
                message: bytes.clone(),
                change:  Some(signed),
                proof:   None,
            }),
            &[],
            None,
            None,
        )
        .unwrap();
        (parent, bytes)
    }

    fn recover_commit(
        provider: &PromtuzMlsProvider, gid: [u8; 32], parent: [u8; 32], bytes: &[u8],
    ) {
        use crate::mls::recovery::Candidate;
        use crate::mls::recovery::Transaction;
        let mut tx = Transaction::open(provider, gid, Some(parent)).unwrap().unwrap();
        let processed = tx
            .group
            .process_incoming(
                &tx.provider,
                mls_message_from_bytes(bytes).unwrap().try_into_protocol_message().unwrap(),
            )
            .unwrap();
        let author = processed.sender;
        let rank = tx.group.group_meta().unwrap().effective().role(&author);
        let CommitOutcome::Merged(changed) = tx
            .group
            .merge_staged_commit_if_permitted(&tx.provider, commit_of(processed.content), author)
            .unwrap()
        else {
            panic!("valid commit refused")
        };
        tx.publish(
            Some(Candidate {
                rank,
                message: bytes.to_vec(),
                change: changed.map(|c| c.signed),
                proof: None,
            }),
            &[],
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn simultaneous_takeovers_converge_after_descendants_and_restart_and_replay_once() {
        use sha2::Digest;
        use sha2::Sha256;

        use crate::mls::recovery::Replay;
        use crate::mls::recovery::Transaction;
        use crate::mls::recovery::replay_needed;
        let pa = build_provider();
        let pb = build_provider();
        let pc = build_provider();
        let alice = Party::new(&pa, 71);
        let bob = Party::new(&pb, 72);
        let carol = Party::new(&pc, 73);
        let gid = [91; 32];
        let mut meta = GroupMeta::founded("recovery".into(), alice.ipk);
        meta.state.as_mut().unwrap().admins = vec![bob.ipk, carol.ipk];
        let mut ga =
            MlsGroupHandle::create(&pa, &alice.sig_kp, alice.cwk(), &gid, Some(&meta)).unwrap();
        let (_, welcome) = ga
            .add_members(&pa, &alice.sig_kp, &[make_kp(&pb, &bob), make_kp(&pc, &carol)])
            .unwrap();
        ga.merge_pending_commit(&pa).unwrap();
        joined(&pb, &welcome);
        joined(&pc, &welcome);
        let b = recovery_commit(&pb, gid, &bob, GroupChange::Takeover);
        let c = recovery_commit(&pc, gid, &carol, GroupChange::Takeover);
        let (winner, winner_provider, winning, loser, losing_provider, losing) =
            if Sha256::digest(&b.1)[..] < Sha256::digest(&c.1)[..] {
                (&bob, &pb, &b, &carol, &pc, &c)
            } else {
                (&carol, &pc, &c, &bob, &pb, &b)
            };
        let losing_child = recovery_commit(
            losing_provider,
            gid,
            loser,
            GroupChange::Rules(GroupRules { members_edit: true, ..GroupRules::default() }),
        );
        let replay = Replay {
            id:         [44; 16],
            payload:    b"a post sent before learning of the collision".to_vec(),
            recipients: vec![alice.ipk, winner.ipk],
            wake:       1,
            kind:       0,
        };
        let mut send = Transaction::open(losing_provider, gid, None).unwrap().unwrap();
        send.group
            .create_application_message(&send.provider, &loser.sig_kp, &replay.payload)
            .unwrap();
        send.publish(None, &[], Some(&replay), None).unwrap();

        // Alice sees the losing branch and its descendant before the winner;
        // the winner receives those same commits in the opposite order.
        recover_commit(&pa, gid, losing.0, &losing.1);
        recover_commit(&pa, gid, losing_child.0, &losing_child.1);
        recover_commit(&pa, gid, winning.0, &winning.1);
        recover_commit(winner_provider, gid, losing.0, &losing.1);
        recover_commit(winner_provider, gid, losing_child.0, &losing_child.1);
        recover_commit(losing_provider, gid, winning.0, &winning.1);
        let expected = MlsGroupHandle::load(winner_provider, &gid).unwrap().unwrap();
        for provider in [&pa, &pb, &pc] {
            // Rebuild both handle and provider from persisted storage.
            let restarted = PromtuzMlsProvider::new(provider.storage().connection());
            let group = MlsGroupHandle::load(&restarted, &gid).unwrap().unwrap();
            assert_eq!(group.branch_id(), expected.branch_id());
            assert_eq!(group.group_meta().unwrap().effective().committer, winner.ipk);
            assert!(!group.group_meta().unwrap().effective().rules.members_edit);
        }
        let pending = replay_needed(losing_provider, &gid).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, replay.id);
        let mut resend = Transaction::open(losing_provider, gid, None).unwrap().unwrap();
        let message = resend
            .group
            .create_application_message(&resend.provider, &loser.sig_kp, &replay.payload)
            .unwrap();
        resend.publish(None, &[], Some(&replay), None).unwrap();
        assert!(replay_needed(losing_provider, &gid).unwrap().is_empty());
        let mut alice_after = Transaction::open(&pa, gid, None).unwrap().unwrap();
        let processed = inbound(&mut alice_after.group, &alice_after.provider, &message);
        let ProcessedMessageContent::ApplicationMessage(body) = processed else {
            panic!("expected post")
        };
        assert_eq!(body.into_bytes(), replay.payload);
        alice_after.publish(None, &[], None, None).unwrap();
        let mut again = Transaction::open(&pa, gid, None).unwrap().unwrap();
        assert!(
            again
                .group
                .process_incoming(
                    &again.provider,
                    mls_message_from_bytes(&mls_message_to_bytes(&message).unwrap())
                        .unwrap()
                        .try_into_protocol_message()
                        .unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn concurrent_operations_and_failed_publication_never_advance_live_ratchets() {
        use crate::mls::recovery::Candidate;
        use crate::mls::recovery::Transaction;
        let provider = build_provider();
        let alice = Party::new(&provider, 81);
        let gid = [92; 32];
        let meta = GroupMeta::founded("transaction".into(), alice.ipk);
        MlsGroupHandle::create(&provider, &alice.sig_kp, alice.cwk(), &gid, Some(&meta)).unwrap();
        let mut first = Transaction::open(&provider, gid, None).unwrap().unwrap();
        let mut stale = Transaction::open(&provider, gid, None).unwrap().unwrap();
        first.group.create_application_message(&first.provider, &alice.sig_kp, b"first").unwrap();
        stale
            .group
            .create_application_message(&stale.provider, &alice.sig_kp, b"concurrent")
            .unwrap();
        first.publish(None, &[], None, None).unwrap();
        assert!(stale.publish(None, &[], None, None).is_err());
        let mut tx = Transaction::open(&provider, gid, None).unwrap().unwrap();
        let before = tx.group.branch_id();
        let signed = signed_by(
            &tx.group,
            &alice,
            GroupChange::Rules(GroupRules { members_send: false, ..GroupRules::default() }),
        );
        let mut meta = tx.group.group_meta().unwrap();
        meta.state = Some(tx.group.state_after_change(&alice.ipk, &signed).unwrap());
        let (commit, _) =
            tx.group.commit_meta(&tx.provider, &alice.sig_kp, &meta, vec![], vec![]).unwrap();
        tx.group.merge_pending_commit(&tx.provider).unwrap();
        provider.storage().connection().lock().execute_batch("CREATE TRIGGER refuse_branch BEFORE INSERT ON mls_branches BEGIN SELECT RAISE(ABORT,'disk failure'); END;").unwrap();
        assert!(
            tx.publish(
                Some(Candidate {
                    rank:    2,
                    message: mls_message_to_bytes(&commit).unwrap(),
                    change:  Some(signed),
                    proof:   None,
                }),
                &[],
                None,
                None
            )
            .is_err()
        );
        assert_eq!(MlsGroupHandle::load(&provider, &gid).unwrap().unwrap().branch_id(), before);
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
}
