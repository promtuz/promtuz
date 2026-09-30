//! Publicly verifiable group history, transported only inside MLS encryption.
//! New joiners can verify recovery without possessing any pre-join secrets.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::KeyInit;
use chacha20poly1305::Nonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::Payload;
use common::proto::mls_wire::GroupBranch;
use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::group_change_signing_input;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use openmls_traits::OpenMlsProvider;
use openmls_traits::random::OpenMlsRand;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::GroupState;
use super::MlsGroupHandle;
use super::PromtuzMlsProvider;
use super::policy;

pub const COMMIT_LABEL: &str = "promtuz recovery commit proof v1";
pub const INVITATION_LABEL: &str = "promtuz recovery invitation history v1";

pub fn verify_member_request(
    gid: &[u8; 32], request: &common::proto::mls_wire::GroupMemberRequest,
) -> Result<()> {
    let input = common::proto::mls_wire::group_member_request_signing_input(
        gid,
        &request.who.0,
        &request.nonce.0,
        &request.action,
    );
    VerifyingKey::from_bytes(&request.who.0)?
        .verify_strict(&input, &Signature::from_bytes(&request.signature.0))?;
    Ok(())
}

pub fn member_requests(
    history: &[GroupBranch],
) -> Result<Vec<common::proto::mls_wire::GroupMemberRequest>> {
    let mut requests = Vec::new();
    for proof in history {
        let state: PublicState = postcard::from_bytes(&proof.context.0)?;
        if let Some(GroupState { last: Some(signed), .. }) = state.state
            && let GroupChange::MemberRequest(request) = signed.change
            && !requests.contains(&request)
        {
            requests.push(request);
        }
    }
    Ok(requests)
}

pub fn change_between(parent: &GroupBranch, next: &GroupBranch) -> Result<Option<super::Changed>> {
    let before: PublicState = postcard::from_bytes(&parent.context.0)?;
    let after: PublicState = postcard::from_bytes(&next.context.0)?;
    Ok(after
        .state
        .and_then(|s| s.last)
        .filter(|s| s.branch == parent.branch)
        .map(|signed| super::Changed { signed, before: before.effective() }))
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct PublicState {
    founder: [u8; 32],
    epoch:   u64,
    title:   String,
    roster:  Vec<[u8; 32]>,
    state:   Option<GroupState>,
}

impl PublicState {
    fn read(group: &MlsGroupHandle) -> Result<Self> {
        let meta = group.group_meta().ok_or_else(|| anyhow!("not a group chat"))?;
        let mut roster = group.roster();
        roster.sort();
        Ok(Self {
            founder: meta.founder,
            epoch: group.epoch(),
            title: meta.title,
            roster,
            state: meta.state,
        })
    }
    fn effective(&self) -> GroupState {
        super::GroupMeta {
            title:   self.title.clone(),
            founder: self.founder,
            state:   self.state.clone(),
        }
        .effective()
    }
}

fn transcript(gid: &[u8; 32], parent: Option<&[u8; 32]>, proof: &GroupBranch) -> Result<Vec<u8>> {
    let mut input = b"promtuz recovery branch proof v1".to_vec();
    input.extend_from_slice(gid);
    input.extend_from_slice(parent.unwrap_or(&[0; 32]));
    input.extend(postcard::to_allocvec(&(
        proof.branch,
        proof.rank,
        proof.commit_hash,
        proof.author,
        &proof.context,
    ))?);
    Ok(input)
}

pub fn sign(
    group: &MlsGroupHandle, parent: Option<[u8; 32]>, rank: u8, message: &[u8], signer: &SigningKey,
) -> Result<GroupBranch> {
    let hash: [u8; 32] = Sha256::digest(message).into();
    let mut proof = GroupBranch {
        branch: group.branch_id().into(),
        rank,
        commit_hash: hash.into(),
        author: signer.verifying_key().to_bytes().into(),
        context: postcard::to_allocvec(&PublicState::read(group)?)?.into(),
        signature: [0; 64].into(),
    };
    proof.signature =
        signer.sign(&transcript(&group.group_id(), parent.as_ref(), &proof)?).to_bytes().into();
    Ok(proof)
}

fn signature(gid: &[u8; 32], parent: Option<&[u8; 32]>, proof: &GroupBranch) -> Result<()> {
    VerifyingKey::from_bytes(&proof.author.0)?.verify_strict(
        &transcript(gid, parent, proof)?,
        &Signature::from_bytes(&proof.signature.0),
    )?;
    Ok(())
}

fn transition(gid: &[u8; 32], parent: &GroupBranch, next: &GroupBranch) -> Result<()> {
    signature(gid, Some(&parent.branch.0), next)?;
    let before: PublicState = postcard::from_bytes(&parent.context.0)?;
    let after: PublicState = postcard::from_bytes(&next.context.0)?;
    ensure!(
        after.founder == before.founder && after.epoch == before.epoch + 1,
        "invalid history epoch or founder"
    );
    ensure!(
        after.roster.len() <= super::MAX_GROUP_MEMBERS
            && after.roster.windows(2).all(|p| p[0] < p[1]),
        "invalid history roster"
    );
    let state = before.effective();
    ensure!(
        next.rank == state.role(&next.author.0) && before.roster.contains(&next.author.0),
        "forged committer rank"
    );
    let new_state = after.state.as_ref().ok_or_else(|| anyhow!("history drops signed rules"))?;
    let request = new_state
        .last
        .as_ref()
        .filter(|c| c.epoch == before.epoch && c.branch.0 == parent.branch.0);
    let mut roster = before.roster.clone();
    if let Some(signed) = request {
        let input = group_change_signing_input(
            gid,
            signed.epoch,
            &signed.branch.0,
            &signed.by.0,
            &signed.change,
        );
        VerifyingKey::from_bytes(&signed.by.0)?
            .verify_strict(&input, &Signature::from_bytes(&signed.sig.0))?;
        let expected = if before.state.is_none() {
            ensure!(
                signed.change == GroupChange::Upgrade
                    && signed.by.0 == before.founder
                    && next.author.0 == before.founder,
                "invalid legacy upgrade"
            );
            GroupState { last: Some(signed.clone()), ..GroupState::founded(before.founder) }
        } else {
            policy::apply(&state, &roster, &next.author.0, signed).map_err(|e| anyhow!("{e}"))?
        };
        ensure!(*new_state == expected, "history rewrites the group's rules");
        match &signed.change {
            GroupChange::Add { who } => roster.extend(who.iter().map(|w| w.0)),
            GroupChange::Remove { who } => roster.retain(|m| *m != who.0),
            GroupChange::Leave { .. } => roster.retain(|m| *m != signed.by.0),
            GroupChange::MemberRequest(request) => {
                verify_member_request(gid, request)?;
                if request.action == common::proto::mls_wire::GroupMemberAction::Leave {
                    roster.retain(|m| *m != request.who.0);
                }
            },
            _ => {},
        }
        roster.sort();
    } else {
        ensure!(
            next.author.0 == state.committer && after.state == before.state,
            "unsigned history change"
        );
        // The founder's first commit adds the founding members, before anyone
        // else has a group in which they could request or race a change.
        if before.epoch == 0 && before.roster == [before.founder] && next.author.0 == before.founder
        {
            ensure!(after.roster.contains(&before.founder), "bootstrap removes founder");
            roster = after.roster.clone();
        }
    }
    ensure!(roster == after.roster, "history's roster is not what its request changes");
    Ok(())
}

pub fn verify_history(
    gid: &[u8; 32], history: &[GroupBranch], group: &MlsGroupHandle,
) -> Result<()> {
    ensure!(!history.is_empty() && history.len() <= 4096, "invalid group history length");
    let root = &history[0];
    signature(gid, None, root)?;
    let state: PublicState = postcard::from_bytes(&root.context.0)?;
    ensure!(
        root.author.0 == state.founder && state.roster.contains(&state.founder),
        "history has no authentic founder"
    );
    let expected = super::GroupMeta {
        title:   state.title.clone(),
        founder: state.founder,
        state:   state.state.as_ref().map(|_| GroupState::founded(state.founder)),
    }
    .effective();
    ensure!(state.effective() == expected, "bootstrap grants unsigned roles");
    let mut used = std::collections::HashSet::new();
    for pair in history.windows(2) {
        transition(gid, &pair[0], &pair[1])?;
        let state: PublicState = postcard::from_bytes(&pair[1].context.0)?;
        if let Some(signed) = state.state.and_then(|s| s.last)
            && signed.branch == pair[0].branch
            && let GroupChange::MemberRequest(request) = signed.change
        {
            ensure!(
                used.insert((request.who.0, request.nonce.0)),
                "history replays a member request"
            );
        }
    }
    matches_group(history.last().expect("nonempty"), group)
}

pub fn matches_group(proof: &GroupBranch, group: &MlsGroupHandle) -> Result<()> {
    ensure!(proof.branch.0 == group.branch_id(), "proof identifies another MLS state");
    ensure!(
        postcard::from_bytes::<PublicState>(&proof.context.0)? == PublicState::read(group)?,
        "proof does not describe the authenticated MLS state"
    );
    Ok(())
}

pub fn verify_commit(
    before: &MlsGroupHandle, after: &MlsGroupHandle, proof: &GroupBranch, author: &[u8; 32],
    message: &[u8],
) -> Result<()> {
    ensure!(proof.author.0 == *author, "proof signer is not the MLS committer");
    let hash: [u8; 32] = Sha256::digest(message).into();
    ensure!(proof.commit_hash.0 == hash, "proof is for another commit");
    // Only the parent context is needed for transition validation here; the
    // receiver already obtained that context through authenticated MLS.
    let parent = GroupBranch {
        branch:      before.branch_id().into(),
        rank:        0,
        commit_hash: [0; 32].into(),
        author:      [0; 32].into(),
        signature:   [0; 64].into(),
        context:     postcard::to_allocvec(&PublicState::read(before)?)?.into(),
    };
    transition(&before.group_id(), &parent, proof)?;
    matches_group(proof, after)
}

pub fn seal(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, label: &str, plaintext: &[u8],
) -> Result<Vec<u8>> {
    let key =
        zeroize::Zeroizing::new(group.export_secret(provider, label, &group.branch_id(), 32)?);
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow!("invalid exporter key"))?;
    let nonce: [u8; 12] =
        provider.rand().random_array().map_err(|e| anyhow!("random nonce: {e:?}"))?;
    let mut output = nonce.to_vec();
    let compressed = lz4_flex::compress_prepend_size(plaintext);
    output.extend(
        cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: &compressed, aad: &group.group_id() },
            )
            .map_err(|_| anyhow!("encrypt history"))?,
    );
    if label == INVITATION_LABEL {
        ensure!(
            output.len() <= common::proto::mls_wire::MAX_WELCOME_BYTES,
            "group change history exceeds the supported invitation size"
        );
    }
    Ok(output)
}

pub fn open(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, label: &str, bytes: &[u8],
) -> Result<Vec<u8>> {
    ensure!(bytes.len() >= 28, "truncated encrypted history");
    let key =
        zeroize::Zeroizing::new(group.export_secret(provider, label, &group.branch_id(), 32)?);
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow!("invalid exporter key"))?;
    let compressed = cipher
        .decrypt(
            Nonce::from_slice(&bytes[..12]),
            Payload { msg: &bytes[12..], aad: &group.group_id() },
        )
        .map_err(|_| anyhow!("invalid encrypted history"))?;
    let length = compressed.get(..4).ok_or_else(|| anyhow!("truncated history"))?;
    ensure!(
        u32::from_le_bytes(length.try_into()?) <= 64 * 1024 * 1024,
        "expanded history too large"
    );
    Ok(lz4_flex::decompress_size_prepended(&compressed)?)
}
