//! Sticker references, signed manifests and gateway uploads. A pack's permanent token is shared
//! with its recipients; the gateway verifies signatures without it.

use serde::Deserialize;
use serde::Serialize;

use crate::proto::pack::bounded_vec;
use crate::types::bytes::Bytes;

pub const STICKER_MAX_BYTES: usize = 256 * 1024;
/// A stored blob is nonce + ciphertext + tag around the sticker bytes.
pub const BLOB_MAX_BYTES: usize = STICKER_MAX_BYTES + 64;
pub const PACK_MAX_STICKERS: usize = 100;
pub const MAX_PACKS_PER_CREATOR: usize = 5;
/// Longest edge of a sticker, in pixels; the other edge fits inside it.
pub const STICKER_EDGE: u32 = 512;
pub const PACK_NAME_MAX: usize = 64;
pub const MANIFEST_MAX_BYTES: usize = 16 * 1024;

const MANIFEST_SIG_DOMAIN: &[u8] = b"promtuz-sticker-manifest-v1";
const BLOB_PUT_SIG_DOMAIN: &[u8] = b"promtuz-sticker-blob-v1";
const BLOB_KEY_DOMAIN: &[u8] = b"promtuz-sticker-key-v1";

/// The serialized layout is stored and fixed: pack 0..16, id 16..48, token 48..80.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerRef {
    pub pack: [u8; 16],
    /// `BLAKE3(plaintext)`: the fetch key's input and the integrity check.
    pub id: [u8; 32],
    pub token: [u8; 32],
    pub store: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSticker {
    pub id: [u8; 32],
    pub width: u16,
    pub height: u16,
}

/// Encrypted under the pack token. `version` grows on every append, and a reader never goes back.
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

/// At `packs/<pack>/manifest`. The gateway checks ownership and version without decrypting;
/// clients pin `creator` on install.
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
        /// [`blob_key`] of the sticker: the object name, opaque to the gateway.
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
    NotOwner,
    /// An older version, or different contents at the stored version.
    StaleVersion,
    TooLarge,
    QuotaExceeded,
    WrongStore,
    /// The backend refused or was unreachable; retry later.
    Unavailable,
    /// The pack exceeds its blob limit or omits previously published blobs.
    PackFull,
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

pub fn blob_key(token: &[u8; 32], id: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(BLOB_KEY_DOMAIN);
    h.update(token);
    h.update(id);
    *h.finalize().as_bytes()
}

pub fn manifest_path(pack: &[u8; 16]) -> String {
    format!("packs/{}/manifest", hex::encode(pack))
}

pub fn blob_path(pack: &[u8; 16], key: &[u8; 32]) -> String {
    format!("packs/{}/b/{}", hex::encode(pack), hex::encode(key))
}

/// Binds pack, store, version and the exact ciphertext, so a stored envelope can be neither
/// re-versioned nor given another blob.
pub fn manifest_signing_input(
    pack_id: &[u8; 16], store: u16, version: u32, manifest_blob: &[u8],
) -> Vec<u8> {
    [
        MANIFEST_SIG_DOMAIN,
        pack_id,
        &store.to_be_bytes(),
        &version.to_be_bytes(),
        blake3::hash(manifest_blob).as_bytes(),
    ]
    .concat()
}

/// Binds the blob's pack, store, object key and contents. Exact replays are safe.
pub fn blob_put_signing_input(
    pack_id: &[u8; 16], store: u16, key: &[u8; 32], bytes: &[u8],
) -> Vec<u8> {
    [BLOB_PUT_SIG_DOMAIN, pack_id, &store.to_be_bytes(), key, blake3::hash(bytes).as_bytes()]
        .concat()
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

    pub fn verify(&self) -> bool {
        self.manifest_blob.len() <= MANIFEST_MAX_BYTES
            && crate::crypto::verify_ed25519(
                &self.creator.0,
                &manifest_signing_input(
                    &self.pack_id,
                    self.store,
                    self.version,
                    &self.manifest_blob,
                ),
                &self.sig.0,
            )
            .is_ok()
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

    /// Checks signatures only; size caps are the caller's.
    pub fn verify(&self) -> bool {
        match self {
            Self::PutBlob { pack, store, key, creator, bytes, sig } => {
                let msg = blob_put_signing_input(pack, *store, key, bytes);
                crate::crypto::verify_ed25519(&creator.0, &msg, &sig.0).is_ok()
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

    #[test]
    fn transcripts() {
        crate::proto::golden(
            &[
                manifest_signing_input(&[1; 16], 0x0102, 0x03040506, b"manifest"),
                blob_put_signing_input(&[1; 16], 0x0102, &[2; 32], b"blob"),
            ],
            "b177b1e283cb1ec80cfacc9ae8e1383de16e2e8490cf282b806a4df84852066b
             23e34835d078a6c294cf0afbd0232e647d2298253b7f4cf03592ca81eb8934fe",
        );
    }
}
