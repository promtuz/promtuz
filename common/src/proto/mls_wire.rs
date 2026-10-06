//! MLS wire types: the envelopes around openmls bytes, KeyPackage and Welcome RPCs, and their
//! signing transcripts. openmls values travel verbatim as `ByteVec`; every domain is distinct.

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::proto::RelayId;
use crate::types::bytes::ByteVec;
use crate::types::bytes::Bytes;

/// Version in the MLS signing transcripts. A bump is a relay flag day, since the relay verifies
/// against its own copy. [`AppPayload`] does not depend on it.
pub const MLS_WIRE_VERSION: u16 = 12;

/// The decrypted MLS plaintext. `target` and `reply_to` name messages by dispatch_id.
/// Ordinals are wire format: append only, and retired variants keep their slots.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AppPayload {
    Text(String),
    Receipt {
        kind: ReceiptKind,
        upto: [u8; 16],
    },
    Edit {
        target:  [u8; 16],
        content: String,
    },
    Delete {
        target: [u8; 16],
    },
    /// The reactor is the MLS sender, never named in the payload.
    React {
        target: [u8; 16],
        emoji:  String,
        add:    bool,
    },
    Reply {
        reply_to: [u8; 16],
        content:  String,
    },
    /// The invitee's first message after accepting a Welcome; arriving at all proves the pair.
    PairAck,
    /// Retired: keeps its ordinal, and receivers ignore it.
    P2p {
        candidates: Vec<SocketAddr>,
        relay:      Option<SocketAddr>,
        token:      [u8; 16],
        disco_key:  [u8; 32],
    },
    Image {
        caption:  String,
        group_id: Option<[u8; 16]>,
        mime:     String,
        width:    u32,
        height:   u32,
        data:     Vec<u8>,
    },
    Attachment {
        caption:  String,
        group_id: Option<[u8; 16]>,
        mime:     String,
        name:     String,
        size:     u64,
        thumb:    Vec<u8>,
        file_id:  [u8; 32],
    },
    /// Asks an offline sender to come online and serve `file_id`. Routed, never stored.
    FileWant {
        file_id: [u8; 32],
    },
    /// Supersedes `Text`, `Reply`, `Image` and `Attachment`.
    Post {
        reply_to: Option<[u8; 16]>,
        body:     Body,
    },
    /// Supersedes `Edit`; libcore decides which body swaps are legal.
    Revise {
        target: [u8; 16],
        body:   Body,
    },
    System(SystemEvent),
    /// Retired personal-profile controls: kept for decoding queued packets and stable ordinals.
    /// These no longer update profile state; use the encrypted profile store.
    Profile {
        name: String,
    },
    /// Retired; see `Profile`.
    Avatar {
        /// Owner-issued revision, shared across chats; removals carry one too.
        revision: u64,
        avif: Option<Vec<u8>>,
    },
    /// Retired; see `Profile`.
    AvatarSync {
        known_revision: Option<u64>,
        reply: bool,
    },
    /// Retired; see `Profile`.
    AvatarAck {
        revision: u64,
    },
    /// `token` and `disco_key` are exchanged, not derived, so both ends agree whatever the epoch.
    /// The answer echoes `session` as `in_reply_to`; an offer past `expires_at_ms` is dropped.
    P2pOffer {
        session:       [u8; 16],
        in_reply_to:   Option<[u8; 16]>,
        expires_at_ms: u64,
        candidates:    Vec<SocketAddr>,
        relay:         Option<SocketAddr>,
        token:         [u8; 16],
        disco_key:     [u8; 32],
    },
    Call(CallMsg),
    /// Retired; see `Profile`.
    ProfileDetails {
        revision: u64,
        name:     String,
        bio:      String,
        card:     Vec<u8>,
    },
    /// Retired; see `Profile`.
    ProfileDetailsSync {
        known_revision: Option<u64>,
        reply:          bool,
    },
    /// Retired; see `Profile`.
    ProfileDetailsAck {
        revision: u64,
    },
    /// Only the group's active admins may change its picture.
    GroupPicture {
        revision: u64,
        avif:     Option<Vec<u8>>,
    },
    /// Lets original group recipients help deliver a post's attachment. It never grants access
    /// without the matching live post.
    AttachmentSharing(AttachmentSharing),
    /// Exact acknowledgements with recipient-observed times, sent only to the original author.
    ReceiptDetails(ReceiptDetails),
    /// The sender deleted this direct chat and its pair group; the receiver drops its copy too.
    Unpaired,
    /// Sent to the committer alone, the one device that changes the group.
    GroupRequest(GroupRequest),
    /// Retired: keeps its ordinal. Roles come only from the signed group context.
    GroupAdmins {
        admins: Vec<Bytes<32>>,
    },
    /// The committer's Welcome for `who`, sealed and delivered by the member who asked to add
    /// them, since `who` accepts a group Welcome only from a contact.
    GroupWelcome {
        who:     Bytes<32>,
        kp_ref:  Bytes<32>,
        welcome: Vec<u8>,
    },
    GroupInvitation {
        who:     Bytes<32>,
        kp_ref:  Bytes<32>,
        welcome: Vec<u8>,
        history: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupRequest {
    /// Retired unsigned requests; they keep their ordinals.
    Add {
        who: Bytes<32>,
    },
    Remove {
        who: Bytes<32>,
    },
    /// A new member asks for what reached them before their Welcome did, and
    /// so was dropped: the group's name and the committer's profile.
    Sync,
    /// A change its author signed, for the committer to carry.
    Change(SignedChange),
    /// A member of a group founded before its rules were signed can follow
    /// them. The founder converts the group once every member has said so.
    Ready,
    /// Supports authenticated branch recovery, including fresh-key rejoining.
    RecoveryReady,
}

/// The committer records it in the signed group state with its commit, and every member checks
/// it against the rules before applying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupChange {
    Add {
        who: Vec<Bytes<32>>,
    },
    Remove {
        who: Bytes<32>,
    },
    /// The signer leaves. `successor` becomes an owner when the signer is the
    /// last one.
    Leave {
        successor: Option<Bytes<32>>,
    },
    /// `role` is 0 for a member, 1 for an admin and 2 for an owner.
    Role {
        who:  Bytes<32>,
        role: u8,
    },
    Rules(GroupRules),
    /// The committer passes its role to `to`.
    Handover {
        to: Bytes<32>,
    },
    /// An admin takes the committer's role, when the committer's phone has
    /// been gone too long.
    Takeover,
    /// The founder converts a group founded before its rules were signed.
    Upgrade,
    /// Replace a member's lost/stale MLS leaf with a fresh identity-bound KP.
    /// The member's separate signature authorizes this across epoch changes.
    MemberRequest(GroupMemberRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupMemberAction {
    Refresh,
    Leave,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMemberRequest {
    pub who:       Bytes<32>,
    pub nonce:     Bytes<16>,
    pub action:    GroupMemberAction,
    pub signature: Bytes<64>,
}

pub fn group_member_request_signing_input(
    group: &[u8; 32], who: &[u8; 32], nonce: &[u8; 16], action: &GroupMemberAction,
) -> Vec<u8> {
    let action = postcard::to_allocvec(action).expect("member action");
    [b"promtuz group member request v1".as_slice(), group, who, nonce, &action].concat()
}

/// What members who aren't admins may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRules {
    pub members_add:    bool,
    pub members_edit:   bool,
    pub members_send:   bool,
    /// Admins may make others admins. Only owners change this.
    pub admins_appoint: bool,
}

impl Default for GroupRules {
    fn default() -> Self {
        Self {
            members_add:    true,
            members_edit:   false,
            members_send:   true,
            admins_appoint: false,
        }
    }
}

/// A [`GroupChange`] signed by the member who asked for it, at the epoch it
/// applies to, so the committer can neither forge a request nor replay one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedChange {
    pub by:     Bytes<32>,
    pub epoch:  u64,
    pub branch: Bytes<32>,
    pub change: GroupChange,
    /// Ed25519 by `by`'s identity key over [`group_change_signing_input`].
    pub sig:    Bytes<64>,
}

pub const GROUP_CHANGE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 group-change";

pub fn group_change_signing_input(
    group_id: &[u8; 32], epoch: u64, branch: &[u8; 32], by: &[u8; 32], change: &GroupChange,
) -> Result<Vec<u8>, postcard::Error> {
    let change = postcard::to_allocvec(change)?;
    Ok([GROUP_CHANGE_SIG_DOMAIN, group_id, &epoch.to_be_bytes(), branch, by, &change].concat())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptEntry {
    pub message_id: [u8; 16],
    pub delivered_at: Option<u64>,
    pub read_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptDetails {
    #[serde(deserialize_with = "receipt_entries")]
    pub entries: Vec<ReceiptEntry>,
}

fn receipt_entries<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ReceiptEntry>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = Vec<ReceiptEntry>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("1 to 128 receipt entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self, mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            if seq.size_hint().is_some_and(|n| n > 128) { return Err(A::Error::custom("too many receipts")); }
            let mut entries = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(128));
            while let Some(entry) = seq.next_element()? {
                if entries.len() == 128 { return Err(A::Error::custom("too many receipts")); }
                entries.push(entry);
            }
            if entries.is_empty() { return Err(A::Error::custom("empty receipts")); }
            Ok(entries)
        }
    }
    d.deserialize_seq(Entries)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentSharing {
    pub message_id: [u8; 16],
    pub file_id: [u8; 32],
    pub size: u64,
    pub expires_at: u64,
    /// Sorted, unique IPKs from the original send, excluding its author.
    #[serde(deserialize_with = "sharing_recipients")]
    pub recipients: Vec<[u8; 32]>,
}

fn sharing_recipients<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<[u8; 32]>, D::Error> {
    struct Recipients;
    impl<'de> serde::de::Visitor<'de> for Recipients {
        type Value = Vec<[u8; 32]>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most 255 original recipients")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self, mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            if seq.size_hint().is_some_and(|n| n > 255) { return Err(A::Error::custom("sharing audience too large")); }
            let mut peers = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(255));
            while let Some(peer) = seq.next_element()? {
                if peers.len() == 255 { return Err(A::Error::custom("sharing audience too large")); }
                peers.push(peer);
            }
            Ok(peers)
        }
    }
    d.deserialize_seq(Recipients)
}

/// One step of a call. Every variant names its call, so a message that
/// outlived the call it belongs to is dropped instead of steering the next.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CallMsg {
    /// Rings the peer with our ICE credentials, DTLS fingerprint and SSRCs (`video_ssrc` is zero
    /// for audio). Sent with the call wake class; dead once `expires_at_ms` passes.
    Offer {
        call:          [u8; 16],
        expires_at_ms: u64,
        video:         bool,
        ufrag:         String,
        pwd:           String,
        fingerprint:   [u8; 32],
        ssrc:          u32,
        video_ssrc:    u32,
        candidates:    Vec<CallCandidate>,
    },
    /// The peer's phone is ringing, so the caller can play ringback.
    Ringing { call: [u8; 16] },
    /// The peer picked up: their half of the ICE and DTLS parameters.
    /// `video_ssrc` is zero unless this is a video call.
    Answer {
        call:        [u8; 16],
        ufrag:       String,
        pwd:         String,
        fingerprint: [u8; 32],
        ssrc:        u32,
        video_ssrc:  u32,
        candidates:  Vec<CallCandidate>,
    },
    /// An address found after the offer or answer went out.
    Candidate { call: [u8; 16], candidate: CallCandidate },
    /// The sender muted or unmuted. On the signaling channel, so it survives a media reconnect.
    Media { call: [u8; 16], muted: bool },
    /// The sender turned their camera on or off; rides the channel like [`Self::Media`].
    Camera { call: [u8; 16], on: bool },
    /// Fresh ICE credentials after a network change. A side that sees a higher `generation` than
    /// its own restarts too and answers, so simultaneous restarts settle on one.
    Restart {
        call:       [u8; 16],
        generation: u32,
        ufrag:      String,
        pwd:        String,
        candidates: Vec<CallCandidate>,
    },
    /// The call is over, or never started.
    End { call: [u8; 16], reason: CallEnd },
}

/// One ICE candidate as the peer's agent should see it. Structured rather
/// than the SDP line so nothing parses text on the signaling path.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CallCandidate {
    Host { addr: SocketAddr },
    ServerReflexive { addr: SocketAddr, base: SocketAddr },
    Relayed { addr: SocketAddr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CallEnd {
    /// Hung up by whoever sent it. Before the answer, this is the caller
    /// giving up, which the callee records as a missed call.
    Hangup,
    Declined,
    /// The callee was already in a call. Sent without ringing.
    Busy,
    Unanswered,
    /// The media path never came up, or dropped and did not recover.
    Failed,
}

/// The actor is the MLS sender, so variants name only the target. A group with signed rules
/// narrates membership from its commits and sends only `Titled`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SystemEvent {
    Added {
        who: Bytes<32>,
    },
    Left {
        who: Bytes<32>,
    },
    Removed {
        who: Bytes<32>,
    },
    Titled {
        title: String,
    },
    /// Retired membership narration, kept for its ordinal.
    AddedBy {
        who: Bytes<32>,
        by:  Bytes<32>,
    },
    RemovedBy {
        who: Bytes<32>,
        by:  Bytes<32>,
    },
}

/// Message content, valid in both [`AppPayload::Post`] and [`AppPayload::Revise`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Body {
    Text(String),
    /// Inline: the compressed bytes ride in the frame, so it works offline.
    Image {
        caption:  String,
        group_id: Option<[u8; 16]>,
        mime:     String,
        width:    u32,
        height:   u32,
        data:     Vec<u8>,
    },
    /// Metadata and a blurred thumbnail; the bytes are fetched device-to-device by `file_id`.
    Attachment {
        caption:  String,
        group_id: Option<[u8; 16]>,
        mime:     String,
        name:     String,
        size:     u64,
        thumb:    Vec<u8>,
        file_id:  [u8; 32],
    },
    /// The token also grants access to the pack. Dimensions reserve the bubble before download.
    Sticker {
        pack:   [u8; 16],
        id:     [u8; 32],
        token:  [u8; 32],
        store:  u16,
        width:  u16,
        height: u16,
    },
    /// Inline like `Image`, under the same cap. `waveform` is a few dozen loudness samples
    /// (0 to 255) for the bubble to draw before any decode.
    Voice {
        mime:        String,
        duration_ms: u32,
        waveform:    Vec<u8>,
        data:        Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReceiptKind {
    Delivered,
    Read,
}

/// Envelope layout version, independent of [`MLS_WIRE_VERSION`].
pub const MLS_ENVELOPE_VERSION: u8 = 1;

/// The frame cap minus headroom for the dispatch and envelope around it, so a valid MLS message
/// always fits one [`super::pack::MAX_FRAME_BYTES`] frame.
pub const MAX_FRAMED_MLS_BYTES: usize = super::pack::MAX_FRAME_BYTES - 16 * 1024;

/// Checked on both ends; it stops a hostile member parking a payload under the name of a picture.
pub const MAX_AVATAR_BYTES: usize = 64 * 1024;

/// An envelope further ahead than this is dropped, not buffered, so a member cannot pin memory.
pub const MAX_EPOCH_AHEAD: u64 = 64;

pub const MAX_WELCOME_BYTES: usize = 256 * 1024;

/// Cap on one publish batch and on a home's stash.
pub const KP_STASH_TARGET: usize = 100;

pub const KP_STASH_LOW_WATER: usize = 20;

pub const KP_PER_FETCH: usize = 1;

/// Per `(target_ipk, requester_relay_id)` pair and per replica.
pub const MAX_KP_FETCH_PER_HOUR: u32 = 60;

pub const MAX_KP_SKEW_MS: u64 = 60_000;

/// The home refuses a record whose `expires_at_ms` is further out than this.
pub const KEYPACKAGE_LIFETIME_MS: u64 = 30 * 24 * 3_600_000;

/// The client rotates its stash this often, so a peer hoarding fetches cannot pin it.
pub const KP_SCHEDULED_ROTATION_MS: u64 = 7 * 24 * 3_600_000;

/// Bounds the disk a malicious peer can pin against one recipient's home.
pub const MAX_WELCOMES_PER_RECIPIENT: usize = 32;

/// Matches [`KEYPACKAGE_LIFETIME_MS`]: an older Welcome references an expired KeyPackage anyway.
pub const WELCOME_LIFETIME_MS: u64 = 30 * 24 * 3_600_000;

pub const MAX_WELCOME_ACK_IDS: usize = MAX_WELCOMES_PER_RECIPIENT;

/// Home-generated random id that tells concurrent Welcomes apart; opaque to the recipient.
pub const WELCOME_ID_LEN: usize = 8;

pub const MLS_DOMAIN_PREFIX: &[u8] = b"promtuz-mls-v1";

pub const MLS_ENVELOPE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 envelope";

pub const WELCOME_ENVELOPE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-envelope";

pub const INVITE_SIG_DOMAIN: &[u8] = b"promtuz-mls-v1 invite";

pub const KP_PUBLISH_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-publish";

/// Reserved; a KeyPackage fetch carries no user signature.
pub const KP_FETCH_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-fetch";

pub const KP_RECORD_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-record";

pub const WELCOME_FETCH_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-fetch";

pub const WELCOME_ACK_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-ack";

// Gate-only wrapper domains: the home checks these signatures itself and never forwards them.
pub const KP_FETCH_WRAP_DOMAIN: &[u8] = b"promtuz-mls-v1 kp-fetch-wrap";
pub const WELCOME_PUBLISH_WRAP_DOMAIN: &[u8] = b"promtuz-mls-v1 welcome-publish-wrap";

/// Carried in `DispatchP::payload` and decoded before openmls sees anything. Append variants only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MlsEnvelopeP {
    Application(MlsApplicationEnvelopeP),
    Welcome(WelcomeEnvelopeP),
    /// Not MLS: a plain signed control message on the dispatch channel.
    PairDecline(PairDeclineP),
    /// Retired: still decoded so a queued one can be acknowledged and dropped.
    ContactRequest {
        sender: Bytes<32>, recipient: Bytes<32>, id: Bytes<16>, expires_ms: u64,
        encapsulated: ByteVec, ciphertext: ByteVec, signature: Bytes<64>,
    },
    /// Group traffic bound to one particular MLS epoch state. A numeric epoch
    /// can have competing commits, so recovery also needs this branch identity.
    GroupApplication {
        branch:  Bytes<32>,
        message: MlsApplicationEnvelopeP,
        proof:   Option<ByteVec>,
    },
    /// An invitation with its public branch ancestry, so a member invited onto a losing branch can
    /// compare a replacement without the pre-join MLS secrets.
    GroupWelcome {
        welcome:   WelcomeEnvelopeP,
        history:   ByteVec,
        signature: Bytes<64>,
    },
    GroupMemberRequest {
        group:   Bytes<32>,
        request: GroupMemberRequest,
    },
    /// An identity authorizes upgrading exactly one legacy MLS state.
    GroupMigrationReady {
        group: Bytes<32>,
        branch: Bytes<32>,
        approval: GroupMigrationApproval,
    },
    /// Replacement keys for an existing conversation, authorized by every member of its old state.
    GroupMigrationWelcome {
        group: Bytes<32>,
        branch: Bytes<32>,
        #[serde(deserialize_with = "crate::proto::pack::bounded_vec::<_, _, 256>")]
        approvals: Vec<GroupMigrationApproval>,
        welcome: WelcomeEnvelopeP,
        history: ByteVec,
        signature: Bytes<64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMigrationApproval {
    pub who: Bytes<32>,
    pub signature: Bytes<64>,
}

pub fn group_migration_signing_input(group: &[u8; 32], branch: &[u8; 32]) -> Vec<u8> {
    [b"promtuz legacy group migration v1".as_slice(), group, branch].concat()
}

pub fn group_migration_welcome_signing_input(
    group: &[u8; 32], branch: &[u8; 32], approvals: &[GroupMigrationApproval],
    welcome: &WelcomeEnvelopeP, history: &[u8],
) -> Vec<u8> {
    let invitation = postcard::to_allocvec(&(group, branch, approvals, welcome, history))
        .expect("serializable migration invitation");
    [b"promtuz legacy migration welcome v1".as_slice(), &invitation].concat()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupBranch {
    pub branch:      Bytes<32>,
    pub rank:        u8,
    pub commit_hash: Bytes<32>,
    pub author:      Bytes<32>,
    pub context:     ByteVec,
    pub signature:   Bytes<64>,
}

pub fn group_welcome_signing_input(welcome: &WelcomeEnvelopeP, history: &[u8]) -> Vec<u8> {
    let invitation =
        postcard::to_allocvec(&(welcome, history)).expect("serializable group invitation");
    [b"promtuz group welcome v1".as_slice(), &invitation].concat()
}

pub fn group_envelope_signing_input(
    version: u16, to: &[u8; 32], group: &[u8; 32], epoch: u64, branch: &[u8; 32], ciphertext: &[u8],
) -> Vec<u8> {
    let envelope = envelope_signing_input(version, to, group, epoch, ciphertext);
    [b"promtuz group envelope v1".as_slice(), branch, &envelope].concat()
}

/// The stable application identity survives resealing after a fork. The MLS
/// signature authenticates it, independently of the carrier's transport id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMessage {
    pub id:      Bytes<16>,
    pub payload: ByteVec,
}

/// One copy per recipient. `version`, `group_id` and `epoch` are plaintext, so the relay learns
/// which IPKs share a group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlsApplicationEnvelopeP {
    pub version:     u8,
    pub group_id:    Bytes<32>,
    pub epoch:       u64,
    /// TLS-encoded `openmls::MlsMessageOut`.
    pub mls_message: ByteVec,
    pub sender_sig:  Bytes<64>,
}

/// `sender_sig` binds the group, both IPKs, `kp_ref_used` and the blob, so a captured Welcome
/// cannot move to another recipient, inviter, group or KeyPackage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeEnvelopeP {
    pub version:       u8,
    pub group_id:      Bytes<32>,
    pub sender_ipk:    Bytes<32>,
    pub recipient_ipk: Bytes<32>,
    /// TLS-encoded `openmls::Welcome`.
    pub welcome_blob:  ByteVec,
    /// The recipient's KeyPackageRef this Welcome consumes.
    pub kp_ref_used:   Bytes<32>,
    pub sender_sig:    Bytes<64>,
    /// Only on a pairing Welcome. Outside `sender_sig`: the invite verifies under the recipient's
    /// own IPK, and the name is self-asserted.
    pub pairing:       Option<PairingP>,
}

/// A bearer pairing capability from the issuer's QR: whoever holds it may add the issuer until
/// `expiry_ms`. Only the issuer's own device verifies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub id:        Bytes<16>,
    pub expiry_ms: u64,
    pub sig:       Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingP {
    pub invite:      Invite,
    /// Self-asserted; the recipient bounds its length on accept.
    pub sender_name: String,
}

/// Why an invitee declined a Welcome; the inviter renders the message.
pub const DECLINE_GROUP_BUILD_FAILED: u8 = 0;
pub const DECLINE_KP_CONSUMED: u8 = 1;
pub const DECLINE_USER_REJECTED: u8 = 2;

pub const PAIR_DECLINE_SIG_DOMAIN: &[u8] = b"promtuz-pair-decline-v1";

/// Signed by the decliner, so a relay cannot forge a rejection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairDeclineP {
    pub sender_ipk:    Bytes<32>,
    pub recipient_ipk: Bytes<32>,
    /// One of the `DECLINE_*` reasons.
    pub reason:        u8,
    pub timestamp:     u64,
    pub sig:           Bytes<64>,
}

pub fn pair_decline_signing_input(
    sender_ipk: &[u8; 32], recipient_ipk: &[u8; 32], reason: u8, timestamp: u64,
) -> Vec<u8> {
    [
        PAIR_DECLINE_SIG_DOMAIN,
        &MLS_WIRE_VERSION.to_be_bytes(),
        sender_ipk,
        recipient_ipk,
        &[reason],
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

/// Binding `to_ipk` stops a relay redirecting the envelope.
pub fn envelope_signing_input(
    protocol_version: u16, to_ipk: &[u8; 32], group_id: &[u8; 32], epoch: u64,
    mls_message_bytes: &[u8],
) -> Vec<u8> {
    [
        MLS_ENVELOPE_SIG_DOMAIN,
        &protocol_version.to_be_bytes(),
        to_ipk,
        group_id,
        &epoch.to_be_bytes(),
        blake3::hash(mls_message_bytes).as_bytes(),
    ]
    .concat()
}

pub fn invite_signing_input(protocol_version: u16, id: &[u8; 16], expiry_ms: u64) -> Vec<u8> {
    [INVITE_SIG_DOMAIN, &protocol_version.to_be_bytes(), id, &expiry_ms.to_be_bytes()].concat()
}

pub fn welcome_envelope_signing_input(
    protocol_version: u16, group_id: &[u8; 32], sender_ipk: &[u8; 32], recipient_ipk: &[u8; 32],
    kp_ref_used: &[u8; 32], welcome_blob: &[u8],
) -> Vec<u8> {
    [
        WELCOME_ENVELOPE_SIG_DOMAIN,
        &protocol_version.to_be_bytes(),
        group_id,
        sender_ipk,
        recipient_ipk,
        kp_ref_used,
        blake3::hash(welcome_blob).as_bytes(),
    ]
    .concat()
}

/// Both the wire form and the relay's stored form. `owner_sig` authenticates a record on its own,
/// even after it leaves the batch whose outer signature covered it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageRecord {
    pub ipk:           Bytes<32>,
    /// MLS KeyPackageRef; the home requires 32 bytes.
    pub kp_ref:        ByteVec,
    /// TLS-encoded `openmls::KeyPackage`.
    pub kp_bytes:      ByteVec,
    /// Records past this are filtered on fetch and refused on store.
    pub expires_at_ms: u64,
    /// Also covers `BLAKE3(kp_bytes)`, since the relay cannot derive `kp_ref` from `kp_bytes`.
    pub owner_sig:     Bytes<64>,
}

/// Stable DHT routing/storage prefix for a user's KeyPackage stash.
pub fn key_package_stash_prefix(ipk: &[u8;32]) -> [u8;32] {
    let mut hash=blake3::Hasher::new();
    hash.update(b"kp:");
    hash.update(ipk);
    *hash.finalize().as_bytes()
}

pub fn kp_record_signing_input(
    protocol_version: u16, ipk: &[u8; 32], kp_ref: &[u8], kp_bytes: &[u8], expires_at_ms: u64,
) -> Vec<u8> {
    [
        KP_RECORD_DOMAIN,
        &protocol_version.to_be_bytes(),
        ipk,
        &(kp_ref.len() as u32).to_be_bytes(),
        kp_ref,
        blake3::hash(kp_bytes).as_bytes(),
        &expires_at_ms.to_be_bytes(),
    ]
    .concat()
}

/// Replaces the home's stash. A known `(ipk, kp_ref)` with different bytes is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackagePublishReq {
    /// Must match every record's `ipk`.
    pub ipk:       Bytes<32>,
    pub records:   Vec<KeyPackageRecord>,
    pub timestamp: u64,
    pub sig:       Bytes<64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPackagePublishOutcome {
    /// Already-present identical records are idempotent and take no new slot.
    Stored,
    /// The outer `sig` or a record's `owner_sig` failed, or a record is malformed.
    BadSig,
    /// A record already expired, or `timestamp` is outside [`MAX_KP_SKEW_MS`].
    Expired,
    NotOwner,
    RateLimited,
    /// The batch or the resulting stash exceeds [`KP_STASH_TARGET`].
    TooMany,
    /// A known `(ipk, kp_ref)` arrived with different `kp_bytes`; the stored record stays.
    StaticFieldsConflict,
    /// Storage could not durably accept the publication.
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackagePublishResp {
    pub outcome: KeyPackagePublishOutcome,
}

/// `records_digest` comes from [`kp_publish_records_digest`].
pub fn kp_publish_signing_input(
    protocol_version: u16, ipk: &[u8; 32], records_digest: &[u8; 32], record_count: u32,
    timestamp: u64,
) -> Vec<u8> {
    [
        KP_PUBLISH_DOMAIN,
        &protocol_version.to_be_bytes(),
        ipk,
        &record_count.to_be_bytes(),
        records_digest,
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

/// BLAKE3 over each record's signing input, not its postcard bytes, which are not byte-stable.
pub fn kp_publish_records_digest(protocol_version: u16, records: &[KeyPackageRecord]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for r in records {
        let inp = kp_record_signing_input(
            protocol_version,
            &r.ipk.0,
            &r.kp_ref.0,
            &r.kp_bytes.0,
            r.expires_at_ms,
        );
        hasher.update(&inp);
    }
    *hasher.finalize().as_bytes()
}

/// Pops one KeyPackage. No user signature: `DhtHello` authenticates the requesting relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchReq {
    pub target_ipk:         Bytes<32>,
    /// Must match the connection's `DhtHello` peer; rate limits key off `(target_ipk, this)`.
    pub requester_relay_id: RelayId,
    pub timestamp:          u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPackageFetchOutcome {
    Found(KeyPackageFetchFound),
    NoStash,
    NotOwner,
    RateLimited,
    /// The home could not durably consume the record. Retry remains possible;
    /// this is distinct from an empty stash or an authentication rejection.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchFound {
    pub record:      KeyPackageRecord,
    pub remaining:   u32,
    /// `BLAKE3(target_ipk || credential_ipk || credential_signing_key)`, compared across replicas.
    pub static_hash: Bytes<32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPackageFetchResp {
    pub outcome: KeyPackageFetchOutcome,
}

/// The envelope's own `sender_sig` authenticates the inviter; `DhtHello` authenticates only the
/// forwarding relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomePublishReq {
    pub envelope:  WelcomeEnvelopeP,
    pub timestamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WelcomePublishOutcome {
    /// Duplicates get distinct rows; the recipient dedupes by `(group_id, kp_ref_used)`.
    Stored,
    /// Bad `sender_sig`, mismatched `recipient_ipk`, or a malformed envelope.
    BadSig,
    StaleTimestamp,
    NotOwner,
    QueueFull,
    RateLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomePublishResp {
    pub outcome: WelcomePublishOutcome,
}

/// Authenticated like [`crate::proto::dht_p2p::QueueFetch`]: the user signs, binding the
/// requesting relay, and the home matches it to the connection's `DhtHello` peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchReq {
    pub user_ipk:           Bytes<32>,
    pub requester_relay_id: RelayId,
    pub timestamp:          u64,
    pub user_sig:           Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchResp {
    pub outcome: WelcomeFetchOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WelcomeFetchOutcome {
    Found(WelcomeFetchFound),
    /// Bad signature, requester mismatch or stale timestamp; reveals nothing about the queue.
    BadSig,
    NotOwner,
    RateLimited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeFetchFound {
    pub welcomes: Vec<WelcomeEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeEntry {
    /// Echoed back in [`WelcomeAckReq::welcome_ids`].
    pub welcome_id: Bytes<8>,
    pub envelope:   WelcomeEnvelopeP,
}

/// Authenticated like [`WelcomeFetchReq`] under its own domain, so a fetch signature cannot delete.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeAckReq {
    pub user_ipk:           Bytes<32>,
    pub requester_relay_id: RelayId,
    pub welcome_ids:        Vec<Bytes<8>>,
    pub timestamp:          u64,
    pub user_sig:           Bytes<64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeAckResp {
    /// No per-id detail: a missing id is a no-op, and reporting it would leak what the home holds.
    pub ok: bool,
}

pub fn welcome_fetch_signing_input(
    protocol_version: u16, user_ipk: &[u8; 32], requester_relay_id: &RelayId, timestamp: u64,
) -> Vec<u8> {
    [
        WELCOME_FETCH_DOMAIN,
        &protocol_version.to_be_bytes(),
        user_ipk,
        requester_relay_id.as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

pub fn welcome_ack_signing_input(
    protocol_version: u16, user_ipk: &[u8; 32], requester_relay_id: &RelayId,
    welcome_ids: &[[u8; WELCOME_ID_LEN]], timestamp: u64,
) -> Vec<u8> {
    [
        WELCOME_ACK_DOMAIN,
        &protocol_version.to_be_bytes(),
        user_ipk,
        requester_relay_id.as_bytes(),
        &(welcome_ids.len() as u32).to_be_bytes(),
        blake3::hash(welcome_ids.as_flattened()).as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

pub fn kp_fetch_wrap_signing_input(
    protocol_version: u16, sender_ipk: &[u8; 32], target_ipk: &[u8; 32], timestamp: u64,
) -> Vec<u8> {
    [
        KP_FETCH_WRAP_DOMAIN,
        &protocol_version.to_be_bytes(),
        sender_ipk,
        target_ipk,
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

/// Only the sender and the blob: `sender_sig` already binds the rest.
pub fn welcome_publish_wrap_signing_input(
    protocol_version: u16, sender_ipk: &[u8; 32], welcome_blob: &[u8], timestamp: u64,
) -> Vec<u8> {
    [
        WELCOME_PUBLISH_WRAP_DOMAIN,
        &protocol_version.to_be_bytes(),
        sender_ipk,
        blake3::hash(welcome_blob).as_bytes(),
        &timestamp.to_be_bytes(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::id::NodeId;

    #[test]
    fn transcripts() {
        let relay = NodeId::from_bytes([9; 32]);
        let welcome = WelcomeEnvelopeP {
            version:       1,
            group_id:      Bytes([1; 32]),
            sender_ipk:    Bytes([2; 32]),
            recipient_ipk: Bytes([3; 32]),
            welcome_blob:  ByteVec(vec![4; 5]),
            kp_ref_used:   Bytes([5; 32]),
            sender_sig:    Bytes([6; 64]),
            pairing:       None,
        };
        let approval = GroupMigrationApproval { who: Bytes([7; 32]), signature: Bytes([8; 64]) };
        let record = KeyPackageRecord {
            ipk:           Bytes([1; 32]),
            kp_ref:        ByteVec(vec![2; 32]),
            kp_bytes:      ByteVec(vec![3; 7]),
            expires_at_ms: 0x0102,
            owner_sig:     Bytes([4; 64]),
        };
        let role = GroupChange::Role { who: Bytes([4; 32]), role: 2 };
        let leave = GroupMemberAction::Leave;
        let (a, b, c) = ([1; 32], [2; 32], [3; 32]);
        crate::proto::golden(
            &[
                group_member_request_signing_input(&a, &b, &[3; 16], &leave),
                group_change_signing_input(&a, 0x0102, &b, &c, &role).unwrap(),
                group_migration_signing_input(&a, &b),
                group_migration_welcome_signing_input(&a, &b, &[approval], &welcome, b"history"),
                group_welcome_signing_input(&welcome, b"history"),
                group_envelope_signing_input(0x0102, &a, &b, 0x0304, &c, b"ciphertext"),
                pair_decline_signing_input(&a, &b, 3, 0x0102),
                envelope_signing_input(0x0102, &a, &b, 0x0304, b"mls"),
                invite_signing_input(0x0102, &[1; 16], 0x0304),
                welcome_envelope_signing_input(0x0102, &a, &b, &c, &[4; 32], b"welcome"),
                kp_record_signing_input(0x0102, &a, &[2; 3], b"kp", 0x0304),
                kp_publish_records_digest(0x0102, &[record.clone(), record]).to_vec(),
                kp_publish_signing_input(0x0102, &a, &b, 3, 0x0304),
                welcome_fetch_signing_input(0x0102, &a, &relay, 0x0304),
                welcome_ack_signing_input(0x0102, &a, &relay, &[[2; 8], [3; 8]], 0x0304),
                kp_fetch_wrap_signing_input(0x0102, &a, &b, 0x0304),
                welcome_publish_wrap_signing_input(0x0102, &a, b"welcome", 0x0304),
            ],
            "c003c9200f05436c03b89611c64195d2dfe5c7c88f9765b23aff4f163f4d9f93
             447e21a0fe7d3f063fa37e042e602bd610b1e926f0abbac6c4c453e29049159c
             5c0daa3b4b1fc7ce8b0601fe8a91323fe87be88cf8fdad185c2afd1c27c86dfe
             21ab0dae5351976e2390a34687fb71a81ca74d12e9aa52e102654b72237384e8
             e3b3c28c66f9cf79268d6a1c74462aeef5c9bb5e4f2376165289844ea77c1eba
             4d6151355d3db96deaf37b129f7482f0555918f626145c64d2cdc1d72788c8e7
             8ed2552c6870458ca26ef2ce32e4cb78918319d84e4d99dcb5c2c111e2016029
             ae03ebdce72bcb2cb14141980138551507b57ade304a19e2632096942ea0147c
             b1d1ed7b3c0210b81d793fdb170fc84e1823cf6cfcaf836ed47491c14d6f2f4b
             eb4d7af44c5973d7613b5250c8e87012d8b203f774ed9e4d5deb87f879eaeb03
             9640a1a268717fe173fdb5f548e8b25466c5c6a1b761fbf94de1dc7dece04a6f
             3dc6d271bfbf1e927e9fb80bfdd8b7641c06a9d82909599df01055cdef0a2445
             45b0cc8e1be92a717f7859aba7b99afa49b1cae95acb497cbbc479e883ade5ac
             aee9979f078b0d299e37f50793049cfa9a2ccd034001770625fce635e6ba5eb1
             f38d8e9f55ce19391e036d7c421bfe3ab4d04da4bc5e90056c0e2f0be74fd8a0
             39239d71e0b5c805ef6c68646be02939875d9c2a1e6e23598eb6e874a1af2f7b
             172a593524d36dbc94be4a32652e406964db08fa186519998540896bc5826e44",
        );
    }
}
