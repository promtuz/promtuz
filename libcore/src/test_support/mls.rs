//! In-process MLS members, each with its own in-memory MLS database.

use std::sync::Arc;

use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::SignedChange;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::group_change_signing_input;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use openmls::prelude::BasicCredential;
use openmls::prelude::Capabilities;
use openmls::prelude::CredentialWithKey;
use openmls::prelude::KeyPackage;
use openmls::prelude::LeafNodeParameters;
use openmls::prelude::MlsGroupJoinConfig;
use openmls::prelude::MlsMessageOut;
use openmls::prelude::PURE_CIPHERTEXT_WIRE_FORMAT_POLICY;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::ProtocolMessage;
use openmls::prelude::StagedCommit;
use openmls::prelude::StagedWelcome;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::types::SignatureScheme;
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::db::Stores;
use crate::mls::GROUP_META_EXTENSION;
use crate::mls::GroupMeta;
use crate::mls::MLS_PADDING_SIZE;
use crate::mls::MlsGroupHandle;
use crate::mls::PROMTUZ_CIPHERSUITE;
use crate::mls::PromtuzMlsProvider;
use crate::mls::credential::bound_credential;
use crate::mls::group::mls_message_from_bytes;

/// A device: an identity, one leaf signer and its own MLS database.
pub struct Party {
    pub ipk:      [u8; 32],
    pub identity: SigningKey,
    pub leaf:     SignatureKeyPair,
    pub db:       Arc<Mutex<Connection>>,
    pub provider: PromtuzMlsProvider,
}

impl Party {
    pub fn new(seed: u8) -> Self {
        let db = Stores::in_memory(String::new()).mls();
        let provider = PromtuzMlsProvider::new(db.clone());
        let identity = SigningKey::from_bytes(&[seed; 32]);
        let leaf = SignatureKeyPair::new(SignatureScheme::ED25519).unwrap();
        leaf.store(provider.storage()).unwrap();
        Self { ipk: identity.verifying_key().to_bytes(), identity, leaf, db, provider }
    }

    /// The leaf key under a credential its identity signed.
    pub fn credential(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential:    bound_credential(&self.identity, self.leaf.public()).into(),
            signature_key: self.leaf.public().into(),
        }
    }

    /// The bare IPK leaves carried before credentials were bound.
    pub fn legacy_credential(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential:    BasicCredential::new(self.ipk.to_vec()).into(),
            signature_key: self.leaf.public().into(),
        }
    }

    /// A KeyPackage for `credential`, its private half kept in this party's database.
    pub fn key_package(&self, credential: CredentialWithKey) -> KeyPackage {
        KeyPackage::builder()
            .leaf_node_capabilities(Capabilities::new(
                None,
                Some(&[PROMTUZ_CIPHERSUITE]),
                Some(&[GROUP_META_EXTENSION]),
                None,
                None,
            ))
            .build(PROMTUZ_CIPHERSUITE, &self.provider, &self.leaf, credential)
            .unwrap()
            .key_package()
            .clone()
    }

    pub fn kp(&self) -> KeyPackage {
        self.key_package(self.credential())
    }

    pub fn group(&self, gid: &[u8; 32]) -> MlsGroupHandle {
        MlsGroupHandle::load(&self.provider, gid).unwrap().unwrap()
    }

    /// Joins from `welcome` with the app's join settings but none of its Welcome checks.
    pub fn join(&self, welcome: &MlsMessageOut) -> MlsGroupHandle {
        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .padding_size(MLS_PADDING_SIZE)
            .build();
        let welcome = welcome.clone().into_welcome().unwrap();
        let staged =
            StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None).unwrap();
        MlsGroupHandle::wrap(staged.into_group(&self.provider).unwrap())
    }

    /// A commit that only refreshes this party's leaf, left pending.
    pub fn update(&self, group: &mut MlsGroupHandle) -> MlsMessageOut {
        let params = LeafNodeParameters::default();
        group.openmls().self_update(&self.provider, &self.leaf, params).unwrap().into_commit()
    }

    pub fn seal(&self, group: &mut MlsGroupHandle, body: &[u8]) -> MlsMessageOut {
        group.create_application_message(&self.provider, &self.leaf, body).unwrap()
    }

    /// What `group` makes of `msg`, which must decrypt.
    pub fn receive(
        &self, group: &mut MlsGroupHandle, msg: &MlsMessageOut,
    ) -> ProcessedMessageContent {
        group.process_incoming(&self.provider, wire(msg)).unwrap().content
    }

    /// `change`, signed for `group`'s epoch and branch.
    pub fn sign(&self, group: &MlsGroupHandle, change: GroupChange) -> SignedChange {
        let (gid, epoch, branch) = (group.group_id(), group.epoch(), group.branch_id());
        let input = group_change_signing_input(&gid, epoch, &branch, &self.ipk, &change).unwrap();
        SignedChange {
            by: self.ipk.into(),
            epoch,
            branch: branch.into(),
            change,
            sig: self.identity.sign(&input).to_bytes().into(),
        }
    }

    /// `welcome` as this party's envelope to `to`, who joins with `kp`.
    pub fn envelope(
        &self, to: &Party, gid: [u8; 32], welcome: MlsMessageOut, kp: &KeyPackage,
    ) -> WelcomeEnvelopeP {
        let (from, signer) = (self.ipk, &self.identity);
        crate::mls::make_welcome_envelope(welcome, gid, from, to.ipk, kp_ref(kp), signer).unwrap()
    }
}

pub fn kp_ref(kp: &KeyPackage) -> [u8; 32] {
    let reference = kp.hash_ref(&openmls_rust_crypto::RustCrypto::default()).unwrap();
    reference.as_slice().try_into().unwrap()
}

/// `founder` founds `gid`, a pair when `meta` is `None`, adds `members` in one commit, and each
/// joins from the Welcome.
pub fn found<const N: usize>(
    founder: &Party, gid: [u8; 32], meta: Option<&GroupMeta>, members: [&Party; N],
) -> (MlsGroupHandle, [MlsGroupHandle; N]) {
    let (provider, leaf) = (&founder.provider, &founder.leaf);
    let mut group =
        MlsGroupHandle::create(provider, leaf, founder.credential(), &gid, meta).unwrap();
    let (_, welcome) = group.add_members(provider, leaf, &members.map(Party::kp)).unwrap();
    group.merge_pending_commit(provider).unwrap();
    (group, members.map(|m| m.join(&welcome)))
}

/// `msg` as it arrives off the wire.
pub fn wire(msg: &MlsMessageOut) -> ProtocolMessage {
    let bytes = msg.tls_serialize_detached().unwrap();
    mls_message_from_bytes(&bytes).unwrap().try_into_protocol_message().unwrap()
}

pub fn commit_of(content: ProcessedMessageContent) -> StagedCommit {
    match content {
        ProcessedMessageContent::StagedCommitMessage(staged) => *staged,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Every openmls storage row and group size, to compare a database before and after.
#[derive(Debug, PartialEq)]
pub struct Dump(Vec<(Vec<u8>, i64, Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, i64)>);

pub fn dump(conn: &Mutex<Connection>) -> Dump {
    let conn = conn.lock();
    let sql = "SELECT group_id, key_tag, sub_key, value FROM mls_storage ORDER BY 1, 2, 3";
    let rows = crate::db::all(&conn, sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)));
    let sql = "SELECT group_id, total_bytes FROM mls_group_size ORDER BY 1";
    let sizes = crate::db::all(&conn, sql, [], |r| Ok((r.get(0)?, r.get(1)?)));
    Dump(rows.unwrap(), sizes.unwrap())
}

/// Runs `f` while every `event` (say `INSERT ON mls_branches`) fails as a full disk would.
pub fn with_failing_trigger<T>(conn: &Mutex<Connection>, event: &str, f: impl FnOnce() -> T) -> T {
    let fail = "BEGIN SELECT RAISE(ABORT, 'disk failure'); END";
    conn.lock()
        .execute_batch(&format!("CREATE TEMP TRIGGER failing BEFORE {event} {fail}"))
        .unwrap();
    let out = f();
    conn.lock().execute_batch("DROP TRIGGER failing").unwrap();
    out
}
