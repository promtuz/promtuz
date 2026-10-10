//! `MlsGroupHandle`: an openmls group with promtuz's group metadata and commit rules.

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

/// Always set explicitly: the openmls default suite uses AES-128-GCM.
pub const PROMTUZ_CIPHERSUITE: Ciphersuite =
    Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519;

type Result<T> = std::result::Result<T, MlsGroupError>;

/// Deliberately not `Clone`: two copies of one group's state would each advance its ratchets.
#[derive(Debug)]
pub struct MlsGroupHandle {
    inner: MlsGroup,
}

/// Carries [`GroupMeta`] in the group context. 0xF100 is in the RFC 9420 private-use range.
const PROMTUZ_GROUP_META_EXT: u16 = 0xF100;

/// Every KeyPackage must declare this: RFC 9420 only adds a leaf that supports every extension in
/// the group context.
pub const GROUP_META_EXTENSION: ExtensionType = ExtensionType::Unknown(PROMTUZ_GROUP_META_EXT);

/// What its founder says a group is, since a group of two looks like a 1:1 on the wire. It lives in
/// the signed group context, so a relay can neither strip nor forge it, and arrives in the Welcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMeta {
    /// Renames travel as `SystemEvent::Titled`; this is the name a new member starts from.
    pub title:   String,
    /// Read from the context, so every member agrees on it however they joined.
    pub founder: [u8; 32],
    /// `None` identifies an unsupported pre-rules group retained for local history.
    pub state:   Option<GroupState>,
}

/// The part of [`GroupMeta`] every client reads. The state follows it in the same extension, past
/// where older clients stop reading.
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

    /// A group without signed rules is managed by its founder alone.
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

    /// Every GroupContextExtensions proposal has to name its extensions as required, so the group
    /// is founded with them required too.
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

    pub(crate) fn from_extensions(exts: &Extensions<GroupContext>) -> Option<Self> {
        exts.iter().find_map(|e| match e {
            Extension::Unknown(PROMTUZ_GROUP_META_EXT, UnknownExtension(bytes)) => {
                Self::decode(bytes)
            },
            _ => None,
        })
    }
}

pub(crate) fn declares_group_meta(exts: &Extensions<GroupContext>) -> bool {
    exts.iter().any(|e| matches!(e, Extension::Unknown(PROMTUZ_GROUP_META_EXT, _)))
}

/// A change a merged commit made, and the rules it replaced.
#[derive(Debug, Clone)]
pub struct Changed {
    pub signed: SignedChange,
    pub before: GroupState,
}

pub enum CommitOutcome {
    /// Refused on every honest device alike: nothing merged.
    Refused,
    /// Merged. For a group with signed rules, the change it made.
    Merged(Option<Changed>),
}

/// `sender` is bound to the authenticated MLS leaf, the only authority on authorship in a group.
pub struct ProcessedInbound {
    pub sender:  [u8; 32],
    pub content: ProcessedMessageContent,
}

impl MlsGroupHandle {
    /// `None` for `meta` builds a pair (1:1) group.
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
            // Welcomes carry the ratchet tree; there is no other channel to send it.
            .use_ratchet_tree_extension(true);

        if let Some(meta) = meta {
            let exts = meta.extensions()?;
            // The founder's own leaf must declare the extension too: RFC 9420 holds every member,
            // including whoever added the extension, to the same bar.
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

        provider.storage().atomic(|| {
            let mls_group = MlsGroup::new_with_group_id(
                provider,
                signer,
                &create_config,
                GroupId::from_slice(group_id),
                credential_with_key,
            )
            .map_err(MlsGroupError::from_openmls)?;
            Ok(Self { inner: mls_group })
        })
    }

    pub fn load(provider: &PromtuzMlsProvider, group_id: &[u8; 32]) -> Result<Option<Self>> {
        let gid = GroupId::from_slice(group_id);
        provider.storage().atomic(|| {
            let loaded = MlsGroup::load(provider.storage(), &gid).map_err(MlsGroupError::Storage)?;
            let Some(mut inner) = loaded else { return Ok(None) };
            // Some stored configurations omit the ratchet tree from later Welcomes. Normalize
            // them so any member can become committer and invite someone.
            let config = MlsGroupJoinConfig::builder()
                .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
                .padding_size(super::MLS_PADDING_SIZE)
                .use_ratchet_tree_extension(true)
                .sender_ratchet_configuration(
                    *inner.configuration().sender_ratchet_configuration(),
                )
                .build();
            if inner.configuration() != &config {
                inner
                    .set_configuration(provider.storage(), &config)
                    .map_err(MlsGroupError::Storage)?;
            }
            Ok(Some(Self { inner }))
        })
    }

    /// The caller merges the pending commit afterwards.
    pub fn add_members<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, new_members: &[KeyPackage],
    ) -> Result<(MlsMessageOut, MlsMessageOut)> {
        provider.storage().atomic(|| {
            let (commit, welcome, _group_info) = self
                .inner
                .add_members(provider, signer, new_members)
                .map_err(MlsGroupError::from_openmls)?;
            Ok((commit, welcome))
        })
    }

    /// The caller merges the pending commit afterwards.
    pub fn remove_members<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, members: &[LeafNodeIndex],
    ) -> Result<MlsMessageOut> {
        provider.storage().atomic(|| {
            let (commit, _welcome, _group_info) = self
                .inner
                .remove_members(provider, signer, members)
                .map_err(MlsGroupError::from_openmls)?;
            Ok(commit)
        })
    }

    /// Returns a Remove proposal, not a commit; another member must commit it.
    pub fn leave<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S,
    ) -> Result<MlsMessageOut> {
        provider.storage().atomic(|| {
            self.inner.leave_group(provider, signer).map_err(MlsGroupError::from_openmls)
        })
    }

    pub fn create_application_message<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, plaintext: &[u8],
    ) -> Result<MlsMessageOut> {
        provider.storage().atomic(|| {
            self.inner.create_message(provider, signer, plaintext).map_err(MlsGroupError::from_openmls)
        })
    }

    pub fn process_incoming(
        &mut self, provider: &PromtuzMlsProvider, message: ProtocolMessage,
    ) -> Result<ProcessedInbound> {
        let processed = provider.storage().atomic(|| {
            self.inner.process_message(provider, message).map_err(MlsGroupError::from_openmls)
        })?;
        // The author is whoever the authenticated leaf's credential names, not the envelope
        // sender, who in a group is routinely someone else. A leaf that proves nothing is refused.
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

    /// The group's rules, checked by every receiver of every commit so all honest devices reach
    /// the same answer. Returns the verified change, if the commit made one.
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
            let state = meta.state.as_ref().ok_or("unsupported group format")?;
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

    /// Guards both commit creation and receipt. An authenticated transport is not a signature on
    /// the request a committer carries for another member.
    pub fn state_after_change(
        &self, author: &[u8; 32], signed: &SignedChange,
    ) -> std::result::Result<GroupState, &'static str> {
        self.verify_change(signed)?;
        if let GroupChange::MemberRequest(request) = &signed.change {
            super::branch_proof::verify_member_request(&self.group_id(), request)
                .map_err(|_| "invalid member resync authorization")?;
        }
        let meta = self.group_meta().ok_or("not a group chat")?;
        let state = meta.state.as_ref().ok_or("unsupported group format")?;
        policy::apply(state, &self.roster(), author, signed)
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
            AppPayload::GroupInvitation { .. } => state.role(author) >= policy::ROLE_ADMIN,
            AppPayload::System(_)
            | AppPayload::GroupAdmins { .. }
            | AppPayload::GroupWelcome { .. } => false,
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
        )
        .map_err(|_| "the change cannot be encoded")?;
        common::crypto::verify_ed25519(&signed.by.0, &input, &signed.sig.0)
            .map_err(|_| "the change's signature doesn't hold")
    }

    /// A refusal merges nothing and is the same answer on every honest device.
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

    /// Commits `meta` with the adds and removals its change makes, in one commit. The caller
    /// merges the pending commit afterwards.
    pub fn commit_meta<S: Signer>(
        &mut self, provider: &PromtuzMlsProvider, signer: &S, meta: &GroupMeta,
        adds: Vec<KeyPackage>, removes: Vec<LeafNodeIndex>,
    ) -> Result<(MlsMessageOut, Option<MlsMessageOut>)> {
        let extensions = meta.extensions()?;
        provider.storage().atomic(|| {
            let bundle = self
                .inner
                .commit_builder()
                .consume_proposal_store(false)
                .propose_adds(adds)
                .propose_removals(removes)
                .propose_group_context_extensions(extensions)
                .map_err(MlsGroupError::from_openmls)?
                .load_psks(provider.storage())
                .map_err(MlsGroupError::from_openmls)?
                .build(provider.rand(), provider.crypto(), signer, |_| true)
                .map_err(MlsGroupError::from_openmls)?
                .stage_commit(provider)
                .map_err(MlsGroupError::from_openmls)?;
            let (commit, welcome, _group_info) = bundle.into_messages();
            Ok((commit, welcome))
        })
    }

    pub fn merge_staged_commit(
        &mut self, provider: &PromtuzMlsProvider, staged: StagedCommit,
    ) -> Result<()> {
        provider.storage().atomic(|| {
            self.inner.merge_staged_commit(provider, staged).map_err(MlsGroupError::from_openmls)
        })
    }

    /// Drops a commit built but not sent, so the group takes the next one.
    pub fn clear_pending_commit(&mut self, provider: &PromtuzMlsProvider) {
        let dropped = provider.storage().atomic(|| {
            self.inner.clear_pending_commit(provider.storage()).map_err(MlsGroupError::Storage)
        });
        if let Err(e) = dropped {
            log::warn!("GROUP: could not drop an unsent commit: {e:?}");
        }
    }

    pub fn merge_pending_commit(&mut self, provider: &PromtuzMlsProvider) -> Result<()> {
        provider.storage().atomic(|| {
            self.inner.merge_pending_commit(provider).map_err(MlsGroupError::from_openmls)
        })
    }

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
        // No transcript: OpenMLS never updates a removed member's copy, while the new tree and
        // extensions reach every recipient, that member included.
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

    pub fn group_id(&self) -> [u8; 32] {
        let slice = self.inner.group_id().as_slice();
        let mut out = [0u8; 32];
        let copy_len = slice.len().min(32);
        out[..copy_len].copy_from_slice(&slice[..copy_len]);
        out
    }

    pub fn member_count(&self) -> usize {
        self.inner.members().count()
    }

    /// `None` for a pair, or for a group whose metadata this version cannot read.
    pub fn group_meta(&self) -> Option<GroupMeta> {
        // Not `export_group_context()`, which exists only under openmls `test-utils`.
        GroupMeta::from_extensions(self.inner.extensions())
    }

    pub fn members(&self) -> impl Iterator<Item = Member> + '_ {
        self.inner.members()
    }


    /// A group chat rather than a pair. Only group chats require bound credentials.
    pub fn is_group_chat(&self) -> bool {
        declares_group_meta(self.inner.extensions())
    }

    /// `None` for a leaf that proves nothing under this group's rule.
    pub fn member_ipk(&self, m: &Member) -> Option<[u8; 32]> {
        super::credential::member_ipk(m, self.is_group_chat())
    }

    pub fn member_ipk_at(&self, index: LeafNodeIndex) -> Option<[u8; 32]> {
        self.inner.member_at(index).and_then(|m| self.member_ipk(&m))
    }

    /// Everyone whose leaf is bound to an identity, in leaf order.
    pub fn roster(&self) -> Vec<[u8; 32]> {
        let strict = self.is_group_chat();
        self.inner.members().filter_map(|m| super::credential::member_ipk(&m, strict)).collect()
    }

    pub fn member_index_by_ipk(&self, ipk: &[u8; 32]) -> Option<LeafNodeIndex> {
        let strict = self.is_group_chat();
        self.inner
            .members()
            .find(|m| super::credential::member_ipk(m, strict) == Some(*ipk))
            .map(|m| m.index)
    }

    pub fn export_secret(
        &self, provider: &PromtuzMlsProvider, label: &str, context: &[u8], length: usize,
    ) -> Result<Vec<u8>> {
        self.inner
            .export_secret(provider.crypto(), label, context, length)
            .map_err(MlsGroupError::from_openmls)
    }

    pub fn delete(&mut self, provider: &PromtuzMlsProvider) -> Result<()> {
        provider.storage().atomic(|| {
            self.inner.delete(provider.storage()).map_err(MlsGroupError::Storage)
        })
    }

    pub(crate) fn wrap(inner: MlsGroup) -> Self {
        Self { inner }
    }

    /// The openmls group, for tests that act as an old or hostile client would.
    #[cfg(test)]
    pub(crate) fn openmls(&mut self) -> &mut MlsGroup {
        &mut self.inner
    }
}

pub fn mls_message_from_bytes(bytes: &[u8]) -> Result<MlsMessageIn> {
    use openmls::prelude::tls_codec::Deserialize as _;
    MlsMessageIn::tls_deserialize_exact(bytes).map_err(MlsGroupError::from_codec)
}

#[cfg(test)]
mod tests {
    use common::proto::mls_wire::GroupRules;
    use expect_test::expect;
    use openmls::prelude::tls_codec::Serialize as _;
    use openmls_basic_credential::SignatureKeyPair;
    use parking_lot::Mutex;
    use sha2::Digest as _;

    use super::*;
    use crate::mls::recovery::Candidate;
    use crate::mls::recovery::Replay;
    use crate::mls::recovery::Transaction;
    use crate::mls::recovery::operation_lock;
    use crate::mls::recovery::replay_needed;
    use crate::mls::storage::tags;
    use crate::test_support::mls::*;

    /// What `p`'s copy of a group makes of `commit` from `author`.
    fn judge(
        p: &Party, g: &mut MlsGroupHandle, commit: &MlsMessageOut, author: [u8; 32],
    ) -> std::result::Result<Option<SignedChange>, &'static str> {
        let staged = commit_of(p.receive(g, commit));
        g.commit_is_permitted(&staged, author)
    }

    /// `g`'s meta with `state` after `signed`, as its commit would carry it.
    fn meta_after(g: &MlsGroupHandle, state: GroupState, signed: SignedChange) -> GroupMeta {
        GroupMeta {
            state: Some(GroupState { last: Some(signed), ..state }),
            ..g.group_meta().unwrap()
        }
    }



    /// With signed rules only the committer commits, unless an admin takes over, and a commit
    /// does exactly what a change signed by someone entitled to ask for it says.
    #[test]
    fn receivers_hold_every_commit_to_the_signed_rules() {
        let (alice, bob, carol, dave) =
            (Party::new(1), Party::new(2), Party::new(3), Party::new(4));
        let meta = GroupMeta::founded("room".into(), alice.ipk);
        let (mut ga, [mut gb, mut gc]) = found(&alice, [0xAC; 32], Some(&meta), [&bob, &carol]);
        let state = ga.group_meta().unwrap().state.unwrap();

        // Alice commits what `signed` asks and `state` says, drops it, and Carol judges it.
        let mut probe = |ga: &mut MlsGroupHandle, signed, state, adds| {
            let meta = meta_after(ga, state, signed);
            let (commit, _) =
                ga.commit_meta(&alice.provider, &alice.leaf, &meta, adds, vec![]).unwrap();
            ga.clear_pending_commit(&alice.provider);
            judge(&carol, &mut gc, &commit, alice.ipk)
        };
        let add_dave = GroupChange::Add { who: vec![dave.ipk.into()] };
        let asked = bob.sign(&ga, add_dave.clone());
        let mut forged = carol.sign(&ga, add_dave);
        forged.by = bob.ipk.into();
        let quiet = GroupRules { members_send: false, ..state.rules };
        let quieted = GroupState { rules: quiet, ..state.clone() };
        let by_member = bob.sign(&ga, GroupChange::Rules(quiet));
        let by_owner = alice.sign(&ga, GroupChange::Rules(quiet));
        let rows = [
            ("members add", asked.clone(), &state, vec![dave.kp()], true),
            ("it adds who was asked for", asked, &state, vec![], false),
            ("bob never signed it", forged, &state, vec![dave.kp()], false),
            ("members set no rules", by_member, &quieted, vec![], false),
            ("the state is what it makes", by_owner, &state, vec![], false),
        ];
        for (case, signed, state, adds, permitted) in rows {
            assert_eq!(probe(&mut ga, signed, state.clone(), adds).is_ok(), permitted, "{case}");
        }

        let signed = carol.sign(&gc, GroupChange::Rules(quiet));
        let meta = meta_after(&gc, quieted, signed);
        let (commit, _) =
            gc.commit_meta(&carol.provider, &carol.leaf, &meta, vec![], vec![]).unwrap();
        gc.clear_pending_commit(&carol.provider);
        assert!(judge(&bob, &mut gb, &commit, carol.ipk).is_err(), "only the committer commits");
        let update = carol.update(&mut gc);
        gc.clear_pending_commit(&carol.provider);
        assert!(judge(&bob, &mut gb, &update, carol.ipk).is_err(), "not even a key update");

        let role = GroupChange::Role { who: bob.ipk.into(), role: policy::ROLE_ADMIN };
        let promoted = GroupState { admins: vec![bob.ipk], ..state };
        let meta = meta_after(&ga, promoted.clone(), alice.sign(&ga, role));
        let (commit, _) =
            ga.commit_meta(&alice.provider, &alice.leaf, &meta, vec![], vec![]).unwrap();
        for (p, g) in [(&bob, &mut gb), (&carol, &mut gc)] {
            let staged = commit_of(p.receive(g, &commit));
            let outcome =
                g.merge_staged_commit_if_permitted(&p.provider, staged, alice.ipk).unwrap();
            assert!(matches!(outcome, CommitOutcome::Merged(Some(_))));
        }
        ga.merge_pending_commit(&alice.provider).unwrap();
        let taken = GroupState { committer: bob.ipk, ..promoted };
        let meta = meta_after(&gb, taken, bob.sign(&gb, GroupChange::Takeover));
        let (commit, _) = gb.commit_meta(&bob.provider, &bob.leaf, &meta, vec![], vec![]).unwrap();
        assert!(judge(&carol, &mut gc, &commit, bob.ipk).is_ok(), "an admin may take over");
    }

    /// Commits `change` on `party`'s live head through the journal, as its parent and bytes.
    fn journal_commit(party: &Party, gid: [u8; 32], change: GroupChange) -> ([u8; 32], Vec<u8>) {
        let mut tx = Transaction::open(&party.provider, gid, None).unwrap().unwrap();
        let signed = party.sign(&tx.group, change);
        let rank = tx.group.group_meta().unwrap().effective().role(&party.ipk);
        let mut meta = tx.group.group_meta().unwrap();
        meta.state = Some(tx.group.state_after_change(&party.ipk, &signed).unwrap());
        let (commit, _) =
            tx.group.commit_meta(&tx.provider, &party.leaf, &meta, vec![], vec![]).unwrap();
        let message = commit.tls_serialize_detached().unwrap();
        tx.group.merge_pending_commit(&tx.provider).unwrap();
        let parent = tx.parent;
        let candidate =
            Candidate { rank, message: message.clone(), change: Some(signed), proof: None };
        tx.publish(Some(candidate), &[], None, None).unwrap();
        (parent, message)
    }

    /// Applies a commit made on `parent` through `party`'s journal.
    fn journal_receive(party: &Party, gid: [u8; 32], (parent, message): &([u8; 32], Vec<u8>)) {
        let mut tx = Transaction::open(&party.provider, gid, Some(*parent)).unwrap().unwrap();
        let protocol =
            mls_message_from_bytes(message).unwrap().try_into_protocol_message().unwrap();
        let processed = tx.group.process_incoming(&tx.provider, protocol).unwrap();
        let author = processed.sender;
        let rank = tx.group.group_meta().unwrap().effective().role(&author);
        let staged = commit_of(processed.content);
        let outcome =
            tx.group.merge_staged_commit_if_permitted(&tx.provider, staged, author).unwrap();
        let CommitOutcome::Merged(changed) = outcome else { panic!("valid commit refused") };
        let change = changed.map(|c| c.signed);
        let candidate = Candidate { rank, message: message.clone(), change, proof: None };
        tx.publish(Some(candidate), &[], None, None).unwrap();
    }

    /// Competing takeovers delivered in any order leave every member on one branch across a
    /// restart, and the losing branch's post is sent again once and decrypts once.
    #[test]
    fn simultaneous_takeovers_converge_after_descendants_and_restart_and_replay_once() {
        let (alice, bob, carol) = (Party::new(71), Party::new(72), Party::new(73));
        let gid = [91; 32];
        let mut meta = GroupMeta::founded("recovery".into(), alice.ipk);
        meta.state.as_mut().unwrap().admins = vec![bob.ipk, carol.ipk];
        found(&alice, gid, Some(&meta), [&bob, &carol]);
        let b = journal_commit(&bob, gid, GroupChange::Takeover);
        let c = journal_commit(&carol, gid, GroupChange::Takeover);
        let ((winner, winning), (loser, losing)) =
            if sha2::Sha256::digest(&b.1)[..] < sha2::Sha256::digest(&c.1)[..] {
                ((&bob, &b), (&carol, &c))
            } else {
                ((&carol, &c), (&bob, &b))
            };
        let edit = GroupRules { members_edit: true, ..GroupRules::default() };
        let losing_child = journal_commit(loser, gid, GroupChange::Rules(edit));
        let replay = Replay {
            id:         [44; 16],
            payload:    b"a post sent before learning of the collision".to_vec(),
            recipients: vec![alice.ipk, winner.ipk],
            wake:       1,
            kind:       0,
        };
        let mut send = Transaction::open(&loser.provider, gid, None).unwrap().unwrap();
        send.group
            .create_application_message(&send.provider, &loser.leaf, &replay.payload)
            .unwrap();
        send.publish(None, &[], Some(&replay), None).unwrap();

        // Alice sees the losing branch and its descendant before the winner; the winner gets
        // them after its own.
        for commit in [losing, &losing_child, winning] {
            journal_receive(&alice, gid, commit);
        }
        journal_receive(winner, gid, losing);
        journal_receive(winner, gid, &losing_child);
        journal_receive(loser, gid, winning);
        let expected = winner.group(&gid).branch_id();
        for party in [&alice, &bob, &carol] {
            let restarted = PromtuzMlsProvider::new(party.db.clone());
            let group = MlsGroupHandle::load(&restarted, &gid).unwrap().unwrap();
            assert_eq!(group.branch_id(), expected);
            let state = group.group_meta().unwrap().effective();
            assert_eq!(state.committer, winner.ipk);
            assert!(!state.rules.members_edit, "the losing branch's change is gone");
        }

        let pending = replay_needed(&loser.provider, &gid).unwrap();
        assert_eq!(pending.iter().map(|r| r.id).collect::<Vec<_>>(), [replay.id]);
        let mut resend = Transaction::open(&loser.provider, gid, None).unwrap().unwrap();
        let message = resend
            .group
            .create_application_message(&resend.provider, &loser.leaf, &replay.payload)
            .unwrap();
        resend.publish(None, &[], Some(&replay), None).unwrap();
        assert!(replay_needed(&loser.provider, &gid).unwrap().is_empty());
        let mut at_alice = Transaction::open(&alice.provider, gid, None).unwrap().unwrap();
        let content = at_alice.group.process_incoming(&at_alice.provider, wire(&message)).unwrap();
        let ProcessedMessageContent::ApplicationMessage(body) = content.content else {
            panic!("expected the post")
        };
        assert_eq!(body.into_bytes(), replay.payload);
        at_alice.publish(None, &[], None, None).unwrap();
        let mut again = Transaction::open(&alice.provider, gid, None).unwrap().unwrap();
        assert!(again.group.process_incoming(&again.provider, wire(&message)).is_err());
    }

    /// A second operation cannot open while one holds the group, and a publication that fails on
    /// disk leaves the live head where it was.
    #[test]
    fn concurrent_operations_and_failed_publication_never_advance_live_ratchets() {
        let alice = Party::new(81);
        let gid = [92; 32];
        let meta = GroupMeta::founded("transaction".into(), alice.ipk);
        MlsGroupHandle::create(&alice.provider, &alice.leaf, alice.credential(), &gid, Some(&meta))
            .unwrap();
        let mut first = Transaction::open(&alice.provider, gid, None).unwrap().unwrap();
        assert!(Transaction::open(&alice.provider, gid, None).is_err(), "the group is taken");
        first.group.create_application_message(&first.provider, &alice.leaf, b"first").unwrap();
        first.publish(None, &[], None, None).unwrap();

        let before = alice.group(&gid).branch_id();
        with_failing_trigger(&alice.db, "INSERT ON mls_branches", || {
            let mut tx = Transaction::open(&alice.provider, gid, None).unwrap().unwrap();
            let quiet = GroupRules { members_send: false, ..GroupRules::default() };
            let signed = alice.sign(&tx.group, GroupChange::Rules(quiet));
            let mut meta = tx.group.group_meta().unwrap();
            meta.state = Some(tx.group.state_after_change(&alice.ipk, &signed).unwrap());
            let (commit, _) =
                tx.group.commit_meta(&tx.provider, &alice.leaf, &meta, vec![], vec![]).unwrap();
            tx.group.merge_pending_commit(&tx.provider).unwrap();
            let message = commit.tls_serialize_detached().unwrap();
            let candidate = Candidate { rank: 2, message, change: Some(signed), proof: None };
            assert!(tx.publish(Some(candidate), &[], None, None).is_err());
        });
        assert_eq!(alice.group(&gid).branch_id(), before);
        assert!(Transaction::open(&alice.provider, gid, None).unwrap().is_some());
    }

    /// Deleting a chat leaves nothing of its group in any MLS table and touches no other group.
    #[test]
    fn purging_a_group_leaves_no_storage_rows_for_it() {
        let alice = Party::new(1);
        let (live, doomed) = ([0x11; 32], [0x22; 32]);
        let (provider, leaf) = (&alice.provider, &alice.leaf);
        let buffer = crate::mls::EpochCatchupBuffer::new(alice.db.clone());
        for gid in [live, doomed] {
            let meta = GroupMeta::founded("chat".into(), alice.ipk);
            let group =
                MlsGroupHandle::create(provider, leaf, alice.credential(), &gid, Some(&meta))
                    .unwrap();
            crate::mls::recovery::ensure_root(provider, &group, &alice.identity).unwrap();
            buffer.push_dispatch(&group, vec![1; 8], 9, alice.ipk, [1; 16], 1).unwrap();
        }
        // Groups holding openmls rows, group sizes, and each group's journal and buffer rows.
        let tally = || {
            let conn = alice.db.lock();
            let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
            let rows = |gid: &[u8; 32]| -> i64 {
                let tables = ["mls_branches", "mls_recovery_roots", "mls_epoch_ahead"];
                let rows = |table| format!("SELECT COUNT(*) FROM {table} WHERE group_id = ?1");
                tables
                    .map(|t| conn.query_row(&rows(t), [gid], |r| r.get::<_, i64>(0)).unwrap())
                    .iter()
                    .sum()
            };
            (
                count("SELECT COUNT(DISTINCT group_id) FROM mls_storage WHERE group_id <> X''"),
                count("SELECT COUNT(*) FROM mls_group_size"),
                rows(&live),
                rows(&doomed),
            )
        };
        assert_eq!(tally(), (2, 2, 3, 3));
        provider.storage().forget_group(&doomed).unwrap();
        assert_eq!(tally(), (1, 1, 3, 0));
        assert!(MlsGroupHandle::load(provider, &doomed).unwrap().is_none());
        assert!(MlsGroupHandle::load(provider, &live).unwrap().is_some());
        let signer =
            SignatureKeyPair::read(provider.storage(), leaf.public(), SignatureScheme::ED25519);
        assert!(signer.is_some(), "the leaf signer is not the group's");
    }

    /// Handshake and application messages alike travel as PrivateMessage, and short bodies pad
    /// to one size.
    #[test]
    fn every_message_is_private_and_short_bodies_pad_to_one_size() {
        let (alice, bob, carol) = (Party::new(1), Party::new(2), Party::new(3));
        let (mut ga, _) = found(&alice, [0x1F; 32], None, [&bob]);
        let mut sealed =
            |n| alice.seal(&mut ga, &vec![b'x'; n]).tls_serialize_detached().unwrap().len();
        let sizes = [1, 2, 60, 120, 200].map(|n| format!("{n}:{}", sealed(n))).join(" ");
        expect!["1:334 2:334 60:334 120:334 200:590"].assert_eq(&sizes);
        let post = alice.seal(&mut ga, b"post");
        let (commit, _) = ga.add_members(&alice.provider, &alice.leaf, &[carol.kp()]).unwrap();
        ga.clear_pending_commit(&alice.provider);
        let proposal = ga.leave(&alice.provider, &alice.leaf).unwrap();
        for msg in [post, commit, proposal] {
            assert_eq!(wire(&msg).wire_format(), WireFormat::PrivateMessage);
        }
    }

    /// Two threads seal while a third decrypts the peer's messages, each step under the group's
    /// operation lock on a freshly loaded group: no ratchet step is reused or rolled back.
    #[test]
    fn concurrent_seals_and_receives_never_reuse_a_ratchet_step() {
        const ROUNDS: usize = 100;
        let (alice, bob) = (Party::new(51), Party::new(52));
        let gid = [0x52; 32];
        let (_, [mut gb]) = found(&alice, gid, None, [&bob]);
        let from_bob: Vec<_> = (0..ROUNDS).map(|_| bob.seal(&mut gb, b"to alice")).collect();
        let sealed = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for _ in 0..ROUNDS {
                        let _operation = operation_lock(&gid).lock();
                        let msg = alice.seal(&mut alice.group(&gid), b"to bob");
                        sealed.lock().push(msg);
                    }
                });
            }
            s.spawn(|| {
                for msg in &from_bob {
                    let _operation = operation_lock(&gid).lock();
                    alice.receive(&mut alice.group(&gid), msg);
                }
            });
        });
        let sealed = sealed.into_inner();
        assert_eq!(sealed.len(), 2 * ROUNDS);
        for msg in &sealed {
            let content = bob.receive(&mut gb, msg);
            assert!(matches!(content, ProcessedMessageContent::ApplicationMessage(_)));
        }
    }

    /// A merge that fails part way leaves nothing of the new epoch behind, and the same commit
    /// applies on retry.
    #[test]
    fn a_failed_merge_leaves_the_group_whole_and_the_commit_retryable() {
        let (alice, bob) = (Party::new(61), Party::new(62));
        let gid = [0x3C; 32];
        let (mut ga, _) = found(&alice, gid, None, [&bob]);
        let commit = alice.update(&mut ga);
        ga.merge_pending_commit(&alice.provider).unwrap();
        let after = alice.seal(&mut ga, b"in the new epoch");
        let apply = || -> Result<()> {
            let mut group = bob.group(&gid);
            bob.provider.storage().atomic(|| {
                let staged =
                    commit_of(group.process_incoming(&bob.provider, wire(&commit))?.content);
                group.merge_staged_commit(&bob.provider, staged)
            })
        };
        let before = dump(&bob.db);
        // The epoch secrets are written after the new tree and context.
        let mid_merge =
            format!("INSERT ON mls_storage WHEN NEW.key_tag = {}", tags::GROUP_EPOCH_SECRETS);
        assert!(with_failing_trigger(&bob.db, &mid_merge, apply).is_err());
        assert_eq!(dump(&bob.db), before, "nothing of the new epoch is left");
        apply().unwrap();
        let mut gb = bob.group(&gid);
        assert_eq!(gb.epoch(), ga.epoch());
        let content = bob.receive(&mut gb, &after);
        assert!(matches!(content, ProcessedMessageContent::ApplicationMessage(_)));
    }
}
