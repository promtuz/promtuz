//! Sticker references, signed manifests and gateway upload requests.
//! Each pack has a permanent capability key shared with its recipients.
//! The gateway verifies signatures without access to that key.

use serde::Deserialize;
use serde::Serialize;

use crate::proto::pack::bounded_vec;
use crate::types::bytes::Bytes;

/// Maximum encoded size of one sticker.
pub const STICKER_MAX_BYTES: usize = 256 * 1024;
/// A stored blob is nonce + ciphertext + tag around the sticker bytes.
pub const BLOB_MAX_BYTES: usize = STICKER_MAX_BYTES + 64;
pub const PACK_MAX_STICKERS: usize = 100;
pub const MAX_PACKS_PER_CREATOR: usize = 5;
/// Longest edge of a sticker, in pixels; the other edge fits inside it.
pub const STICKER_EDGE: u32 = 512;
pub const PACK_NAME_MAX: usize = 64;
/// An encrypted manifest is a few dozen bytes per sticker plus a name.
pub const MANIFEST_MAX_BYTES: usize = 16 * 1024;

const MANIFEST_SIG_DOMAIN: &[u8] = b"promtuz-sticker-manifest-v1";
const BLOB_PUT_SIG_DOMAIN: &[u8] = b"promtuz-sticker-blob-v1";
const BLOB_KEY_DOMAIN: &[u8] = b"promtuz-sticker-key-v1";

/// Everything needed to fetch and decrypt a sticker.
/// SQL reads the first 48 serialized bytes as `pack ‖ id`; preserve this layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerRef {
    pub pack: [u8; 16],
    /// `BLAKE3(plaintext)` — the fetch key's input and the integrity check.
    pub id: [u8; 32],
    /// The pack token `T`.
    pub token: [u8; 32],
    pub store: u16,
}

/// One entry of a pack manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSticker {
    pub id: [u8; 32],
    pub width: u16,
    pub height: u16,
}

/// The pack's contents, readable only under `T`. `version` counts up on every
/// append; a reader keeps the highest it has seen and refuses to go back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub pack_id: [u8; 16],
    pub store_id: u16,
    pub creator: [u8; 32],
    pub version: u32,
    pub name: String,
    #[serde(deserialize_with = "bounded_vec::<_, _, PACK_MAX_STICKERS>")]
    pub stickers: Vec<ManifestSticker>,
}

/// The mutable object at `packs/<pack>/manifest`.
/// The gateway verifies ownership and version without decrypting the contents.
/// Clients pin `creator` on installation and decrypt with the pack token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEnvelope {
    pub pack_id: [u8; 16],
    pub store: u16,
    pub creator: Bytes<32>,
    pub version: u32,
    #[serde(deserialize_with = "bounded_vec::<_, _, MANIFEST_MAX_BYTES>")]
    pub manifest_blob: Vec<u8>,
    /// By `creator` over [`manifest_signing_input`].
    pub sig: Bytes<64>,
}

/// One upload per bidirectional `client/N` stream.
/// Upload blobs before publishing the manifest that references them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StoreRequest {
    PutBlob {
        pack: [u8; 16],
        store: u16,
        /// [`blob_key`] of the sticker — the object name; opaque to the gateway.
        key: [u8; 32],
        creator: Bytes<32>,
        #[serde(deserialize_with = "bounded_vec::<_, _, BLOB_MAX_BYTES>")]
        bytes: Vec<u8>,
        /// By `creator` over [`blob_put_signing_input`].
        sig: Bytes<64>,
    },
    /// Publish an append-only roster. It must include every previously
    /// published object. Unreferenced uploads are eligible for cleanup.
    PutManifest {
        env: ManifestEnvelope,
        #[serde(deserialize_with = "bounded_vec::<_, _, PACK_MAX_STICKERS>")]
        keys: Vec<Bytes<32>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreReject {
    BadSignature,
    /// The pack belongs to another creator.
    NotOwner,
    /// An older version, or different contents at the stored version.
    StaleVersion,
    TooLarge,
    QuotaExceeded,
    /// This gateway serves a different store id.
    WrongStore,
    /// The backend refused or was unreachable; retry later.
    Unavailable,
    /// The pack exceeds its blob limit or omits previously published blobs.
    PackFull,
    /// The manifest names an object the store does not hold.
    MissingBlob,
}

impl std::fmt::Display for StoreReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreResponse {
    Ok,
    Rejected(StoreReject),
}

/// Derives an opaque object name from the pack token and content id.
pub fn blob_key(token: &[u8; 32], id: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(BLOB_KEY_DOMAIN);
    h.update(token);
    h.update(id);
    *h.finalize().as_bytes()
}

/// Object path of a pack's manifest under a store's base URL.
pub fn manifest_path(pack: &[u8; 16]) -> String {
    format!("packs/{}/manifest", hex::encode(pack))
}

/// Object path of a sticker blob under a store's base URL.
pub fn blob_path(pack: &[u8; 16], key: &[u8; 32]) -> String {
    format!("packs/{}/b/{}", hex::encode(pack), hex::encode(key))
}

/// The bytes a creator signs to publish a manifest. Binds the pack, the
/// version and the exact ciphertext, so a stored envelope can neither be
/// re-versioned nor have its blob swapped.
pub fn manifest_signing_input(
    pack_id: &[u8; 16], store: u16, version: u32, manifest_blob: &[u8],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(MANIFEST_SIG_DOMAIN.len() + 16 + 2 + 4 + 32);
    v.extend_from_slice(MANIFEST_SIG_DOMAIN);
    v.extend_from_slice(pack_id);
    v.extend_from_slice(&store.to_be_bytes());
    v.extend_from_slice(&version.to_be_bytes());
    v.extend_from_slice(blake3::hash(manifest_blob).as_bytes());
    v
}

/// Binds the blob's pack, store, object key and contents. Exact replays are safe.
pub fn blob_put_signing_input(
    pack_id: &[u8; 16], store: u16, key: &[u8; 32], bytes: &[u8],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(BLOB_PUT_SIG_DOMAIN.len() + 16 + 2 + 32 + 32);
    v.extend_from_slice(BLOB_PUT_SIG_DOMAIN);
    v.extend_from_slice(pack_id);
    v.extend_from_slice(&store.to_be_bytes());
    v.extend_from_slice(key);
    v.extend_from_slice(blake3::hash(bytes).as_bytes());
    v
}

#[cfg(feature = "crypto")]
fn verify_by(creator: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    use ed25519_dalek::Signature;
    use ed25519_dalek::VerifyingKey;
    let Ok(vk) = VerifyingKey::from_bytes(creator) else { return false };
    vk.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

#[cfg(feature = "crypto")]
impl ManifestEnvelope {
    pub fn signed(
        key: &ed25519_dalek::SigningKey, pack_id: [u8; 16], store: u16, version: u32,
        manifest_blob: Vec<u8>,
    ) -> Self {
        use ed25519_dalek::Signer;
        let sig = key.sign(&manifest_signing_input(&pack_id, store, version, &manifest_blob));
        Self {
            pack_id,
            store,
            creator: Bytes(key.verifying_key().to_bytes()),
            version,
            manifest_blob,
            sig: Bytes(sig.to_bytes()),
        }
    }

    /// `sig` is by `creator` over this envelope and the blob is within bounds.
    pub fn verify(&self) -> bool {
        self.manifest_blob.len() <= MANIFEST_MAX_BYTES
            && verify_by(
                &self.creator.0,
                &manifest_signing_input(
                    &self.pack_id,
                    self.store,
                    self.version,
                    &self.manifest_blob,
                ),
                &self.sig.0,
            )
    }
}

#[cfg(feature = "crypto")]
impl StoreRequest {
    pub fn signed_blob(
        key: &ed25519_dalek::SigningKey, pack: [u8; 16], store: u16, blob_key: [u8; 32],
        bytes: Vec<u8>,
    ) -> Self {
        use ed25519_dalek::Signer;
        let sig = key.sign(&blob_put_signing_input(&pack, store, &blob_key, &bytes));
        Self::PutBlob {
            pack,
            store,
            key: blob_key,
            creator: Bytes(key.verifying_key().to_bytes()),
            bytes,
            sig: Bytes(sig.to_bytes()),
        }
    }

    /// The signature check for either variant. Size caps are the caller's.
    pub fn verify(&self) -> bool {
        match self {
            Self::PutBlob { pack, store, key, creator, bytes, sig } => {
                verify_by(&creator.0, &blob_put_signing_input(pack, *store, key, bytes), &sig.0)
            },
            Self::PutManifest { env, .. } => env.verify(),
        }
    }

    pub fn creator(&self) -> [u8; 32] {
        match self {
            Self::PutBlob { creator, .. } => creator.0,
            Self::PutManifest { env, .. } => env.creator.0,
        }
    }

    pub fn pack(&self) -> [u8; 16] {
        match self {
            Self::PutBlob { pack, .. } => *pack,
            Self::PutManifest { env, .. } => env.pack_id,
        }
    }

    pub fn store(&self) -> u16 {
        match self {
            Self::PutBlob { store, .. } => *store,
            Self::PutManifest { env, .. } => env.store,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::pack::Packer;
    use crate::proto::pack::Unpacker;

    fn sample_ref() -> StickerRef {
        StickerRef { pack: [1; 16], id: [2; 32], token: [3; 32], store: 7 }
    }

    /// A message row stores these bytes and SQL reads the pack and id back
    /// out of them by offset, so the layout is part of the contract.
    #[test]
    fn sticker_ref_layout_is_pack_then_id() {
        let bytes = sample_ref().ser().unwrap();
        assert_eq!(&bytes[..16], &[1u8; 16]);
        assert_eq!(&bytes[16..48], &[2u8; 32]);
        assert_eq!(&bytes[48..80], &[3u8; 32]);
        assert_eq!(StickerRef::deser(&bytes).unwrap(), sample_ref());
    }

    #[test]
    fn blob_key_depends_on_token_and_id() {
        let a = blob_key(&[1; 32], &[2; 32]);
        assert_eq!(a, blob_key(&[1; 32], &[2; 32]));
        assert_ne!(a, blob_key(&[9; 32], &[2; 32]));
        assert_ne!(a, blob_key(&[1; 32], &[9; 32]));
    }

    #[test]
    fn manifest_round_trips_and_refuses_oversize() {
        let m = Manifest {
            pack_id: [1; 16],
            store_id: 1,
            creator: [4; 32],
            version: 3,
            name: "cats".into(),
            stickers: vec![ManifestSticker { id: [5; 32], width: 512, height: 384 }],
        };
        assert_eq!(Manifest::deser(&m.ser().unwrap()).unwrap(), m);
        let big = Manifest {
            stickers: vec![
                ManifestSticker { id: [0; 32], width: 1, height: 1 };
                PACK_MAX_STICKERS + 1
            ],
            ..m
        };
        assert!(Manifest::deser(&big.ser().unwrap()).is_err());
    }

    #[cfg(feature = "crypto")]
    #[test]
    fn signed_manifest_verifies_until_touched() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let env = ManifestEnvelope::signed(&key, [1; 16], 1, 2, vec![7, 7, 7]);
        assert!(env.verify());
        let env = ManifestEnvelope::deser(&env.ser().unwrap()).unwrap();
        assert!(env.verify());
        let mut bumped = env.clone();
        bumped.version = 3;
        assert!(!bumped.verify());
        let mut swapped = env.clone();
        swapped.manifest_blob = vec![8, 8, 8];
        assert!(!swapped.verify());
        let mut other = env;
        other.creator = Bytes([1; 32]);
        assert!(!other.verify());
    }

    #[cfg(feature = "crypto")]
    #[test]
    fn signed_blob_put_verifies_until_touched() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let req = StoreRequest::signed_blob(&key, [1; 16], 1, [2; 32], vec![1, 2, 3]);
        assert!(req.verify());
        assert_eq!(req.creator(), key.verifying_key().to_bytes());
        let StoreRequest::PutBlob { pack, store, key: k, creator, sig, .. } = req.clone() else {
            unreachable!()
        };
        let forged =
            StoreRequest::PutBlob { pack, store, key: k, creator, bytes: vec![1, 2, 4], sig };
        assert!(!forged.verify());
    }

    #[test]
    fn oversize_blob_fails_to_deserialize() {
        let req = StoreRequest::PutBlob {
            pack: [1; 16],
            store: 1,
            key: [2; 32],
            creator: Bytes([3; 32]),
            bytes: vec![0; BLOB_MAX_BYTES + 1],
            sig: Bytes([0; 64]),
        };
        assert!(StoreRequest::deser(&req.ser().unwrap()).is_err());
    }
}
