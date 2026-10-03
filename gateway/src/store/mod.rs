//! Authenticated uploads of encrypted sticker packs.

pub mod backend;
pub mod ledger;
pub mod s3;

use anyhow::Context;
use anyhow::Result;
use common::info;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::sticker::BLOB_MAX_BYTES;
use common::proto::sticker::MAX_PACKS_PER_CREATOR;
use common::proto::sticker::ManifestEnvelope;
use common::proto::sticker::PACK_MAX_STICKERS;
use common::proto::sticker::StoreReject;
use common::proto::sticker::StoreRequest;
use common::proto::sticker::StoreResponse;
use common::proto::sticker::blob_path;
use common::proto::sticker::manifest_path;
use common::utils::now_secs;
use common::warn;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use crate::config::StoreConfig;
use crate::store::backend::Backend;
use crate::store::ledger::Ledger;
use crate::store::ledger::PackEntry;

/// Allows a replacement upload batch before abandoned blobs are cleaned up.
const UNNAMED_MAX: usize = 2 * PACK_MAX_STICKERS;
const EXPIRY_SECS: u64 = 24 * 60 * 60;
/// Requests waiting for the ledger, each holding up to one blob.
const MAX_PENDING: usize = 64;

pub struct StickerStore {
    id: u16,
    backend: Backend,
    ledger: Mutex<Ledger>,
    pending: Semaphore,
}

impl StickerStore {
    pub fn from_config(cfg: &StoreConfig) -> Result<Self> {
        let backend = Backend::from_config(&cfg.backend)?;
        let ledger = Ledger::open(&cfg.db)?;
        info!(
            "sticker store {} enabled ({}, {} packs on ledger)",
            cfg.id,
            backend.describe(),
            ledger.pack_count()?
        );
        Ok(Self { id: cfg.id, backend, ledger: Mutex::new(ledger), pending: Semaphore::new(MAX_PENDING) })
    }

    pub async fn handle(&self, req: StoreRequest) -> StoreResponse {
        if req.store() != self.id {
            return StoreResponse::Rejected(StoreReject::WrongStore);
        }
        if let StoreRequest::PutBlob { bytes, .. } = &req
            && bytes.len() > BLOB_MAX_BYTES
        {
            return StoreResponse::Rejected(StoreReject::TooLarge);
        }
        if !req.verify() {
            return StoreResponse::Rejected(StoreReject::BadSignature);
        }
        let Ok(_permit) = self.pending.try_acquire() else {
            return StoreResponse::Rejected(StoreReject::Unavailable);
        };
        match self.apply(req).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("sticker store: {e:#}");
                StoreResponse::Rejected(StoreReject::Unavailable)
            },
        }
    }

    async fn apply(&self, req: StoreRequest) -> Result<StoreResponse> {
        let pack = req.pack();
        let creator = req.creator();
        // Publication and cleanup share this lock, including object writes
        // and deletes. A blob cannot become referenced during its deletion.
        let mut ledger = self.ledger.lock().await;

        // Recover missing ownership from the stored manifest. Release the lock
        // during this lookup, then recheck in case another request claimed it.
        let mut known = ledger.get(&pack)?;
        if known.is_none() {
            drop(ledger);
            let stored = match self.backend.get(&manifest_path(&pack)).await? {
                Some(raw) => {
                    Some(ManifestEnvelope::deser(&raw).context("stored manifest does not decode")?)
                },
                None => None,
            };
            ledger = self.ledger.lock().await;
            known = ledger.get(&pack)?;
            if known.is_none()
                && let Some(env) = stored
            {
                anyhow::ensure!(
                    env.pack_id == pack && env.store == self.id && env.verify(),
                    "invalid stored manifest"
                );
                ledger.claim(&pack, &env.creator.0, env.version, now_secs())?;
                known = Some(PackEntry { creator: env.creator.0, version: env.version });
            }
        }
        let version = match known {
            Some(e) if e.creator != creator => {
                return Ok(StoreResponse::Rejected(StoreReject::NotOwner));
            },
            Some(e) => e.version,
            None if ledger.packs_of(&creator)? >= MAX_PACKS_PER_CREATOR => {
                return Ok(StoreResponse::Rejected(StoreReject::QuotaExceeded));
            },
            // Failed uploads must not consume a creator's pack quota.
            None => 0,
        };

        match req {
            StoreRequest::PutBlob { key, bytes, .. } => {
                if ledger.has_blob(&pack, &key)? {
                    return Ok(StoreResponse::Ok);
                }
                // After ledger loss, an existing object may already be published.
                // Leave it intact and recover its reference on the next manifest.
                if version > 0 && self.backend.get(&blob_path(&pack, &key)).await?.is_some() {
                    return Ok(StoreResponse::Ok);
                }
                if ledger.unnamed_count(&pack)? >= UNNAMED_MAX {
                    return Ok(StoreResponse::Rejected(StoreReject::PackFull));
                }
                self.backend.put(&blob_path(&pack, &key), bytes, true).await?;
                ledger.add_blob(&pack, &creator, &key, now_secs())?;
            },
            StoreRequest::PutManifest { env, keys, .. } => {
                let keys: Vec<[u8; 32]> = keys.iter().map(|k| k.0).collect();
                if env.version < version {
                    return Ok(StoreResponse::Rejected(StoreReject::StaleVersion));
                }
                let serialized = env.ser()?;
                // A retry of the stored version changes nothing.
                if env.version == version {
                    let stored = self.backend.get(&manifest_path(&pack)).await?;
                    return Ok(if stored.as_deref() == Some(serialized.as_slice()) {
                        StoreResponse::Ok
                    } else {
                        StoreResponse::Rejected(StoreReject::StaleVersion)
                    });
                }
                if keys.len() > PACK_MAX_STICKERS
                    || ledger.named_keys(&pack)?.iter().any(|k| !keys.contains(k))
                {
                    return Ok(StoreResponse::Rejected(StoreReject::PackFull));
                }
                for key in &keys {
                    if !ledger.has_blob(&pack, key)?
                        && self.backend.get(&blob_path(&pack, key)).await?.is_none()
                    {
                        return Ok(StoreResponse::Rejected(StoreReject::MissingBlob));
                    }
                }
                self.backend.put(&manifest_path(&pack), serialized, false).await?;
                let unnamed = ledger.publish(&pack, &creator, env.version, &keys, now_secs())?;
                self.delete_blobs(&mut ledger, unnamed.into_iter().map(|k| (pack, k)).collect())
                    .await;
            },
        }
        Ok(StoreResponse::Ok)
    }

    /// Delete objects, then forget the ones that are gone. A delete that
    /// failed leaves its row, so the next publish or sweep tries again.
    async fn delete_blobs(&self, ledger: &mut Ledger, blobs: Vec<([u8; 16], [u8; 32])>) {
        let mut gone = Vec::with_capacity(blobs.len());
        for (pack, key) in blobs {
            match self.backend.delete(&blob_path(&pack, &key)).await {
                Ok(()) => gone.push((pack, key)),
                Err(e) => warn!("sticker store: delete {}: {e:#}", blob_path(&pack, &key)),
            }
        }
        for (pack, key) in gone {
            if let Err(e) = ledger.forget(&pack, &key) {
                warn!("sticker store: forget {}: {e:#}", blob_path(&pack, &key));
            }
        }
    }

    /// Delete unpublished packs and unreferenced blobs older than [`EXPIRY_SECS`].
    pub async fn sweep(&self) {
        self.sweep_before(now_secs().saturating_sub(EXPIRY_SECS)).await;
    }

    async fn sweep_before(&self, before: u64) {
        let mut ledger = self.ledger.lock().await;
        let (packs, blobs) = match ledger.expired(before) {
            Ok(x) => x,
            Err(e) => return warn!("sticker store: sweep: {e:#}"),
        };
        if packs.is_empty() && blobs.is_empty() {
            return;
        }
        info!("sticker store: sweeping {} packs, {} blobs", packs.len(), blobs.len());
        self.delete_blobs(&mut ledger, blobs).await;
        for pack in packs {
            if let Err(e) = ledger.drop_pack(&pack) {
                warn!("sticker store: drop {}: {e:#}", hex::encode(pack));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use common::crypto::SigningKey;
    use common::proto::sticker::StoreReject::*;
    use common::types::bytes::Bytes;

    use super::*;
    use crate::config::BackendConfig;

    fn store(dir: &Path) -> StickerStore {
        let backend = BackendConfig::Fs { dir: dir.join("objects") };
        StickerStore::from_config(&StoreConfig { id: 1, db: dir.join("ledger.db"), backend })
            .unwrap()
    }

    fn manifest(key: &SigningKey, pack: [u8; 16], version: u32, keys: &[[u8; 32]]) -> StoreRequest {
        let env = ManifestEnvelope::signed(key, pack, 1, version, vec![version as u8; 8]);
        StoreRequest::signed_manifest(key, env, keys.iter().map(|k| Bytes(*k)).collect())
    }

    fn blob(key: &SigningKey, pack: [u8; 16], k: [u8; 32]) -> StoreRequest {
        StoreRequest::signed_blob(key, pack, 1, k, vec![k[0]])
    }

    fn stored(dir: &Path, pack: [u8; 16], key: [u8; 32]) -> bool {
        dir.join("objects").join(blob_path(&pack, &key)).exists()
    }

    #[tokio::test]
    async fn a_manifest_needs_its_blobs_and_a_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let alice = SigningKey::from_bytes(&[1; 32]);
        let pack = [7; 16];
        assert_eq!(s.handle(blob(&alice, pack, [9; 32])).await, StoreResponse::Ok);
        let missing = manifest(&alice, pack, 1, &[[9; 32], [8; 32]]);
        assert_eq!(s.handle(missing).await, StoreResponse::Rejected(MissingBlob));
        assert_eq!(s.handle(manifest(&alice, pack, 2, &[[9; 32]])).await, StoreResponse::Ok);
        let older = manifest(&alice, pack, 1, &[[9; 32]]);
        assert_eq!(s.handle(older).await, StoreResponse::Rejected(StaleVersion));
        let env = ManifestEnvelope::signed(&alice, pack, 1, 2, b"rewritten".to_vec());
        let rewritten = StoreRequest::signed_manifest(&alice, env, vec![Bytes([9; 32])]);
        assert_eq!(s.handle(rewritten).await, StoreResponse::Rejected(StaleVersion));
        let mut forged = manifest(&alice, pack, 3, &[[9; 32]]);
        if let StoreRequest::PutManifest { keys, .. } = &mut forged {
            keys.clear();
        }
        assert_eq!(s.handle(forged).await, StoreResponse::Rejected(BadSignature));
    }

    #[tokio::test]
    async fn replaying_the_public_manifest_keeps_pending_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let alice = SigningKey::from_bytes(&[1; 32]);
        let pack = [7; 16];
        assert_eq!(s.handle(blob(&alice, pack, [1; 32])).await, StoreResponse::Ok);
        let published = manifest(&alice, pack, 1, &[[1; 32]]);
        assert_eq!(s.handle(published.clone()).await, StoreResponse::Ok);
        assert_eq!(s.handle(blob(&alice, pack, [2; 32])).await, StoreResponse::Ok);

        assert_eq!(s.handle(published).await, StoreResponse::Ok);
        assert!(stored(dir.path(), pack, [2; 32]));
        let next = manifest(&alice, pack, 2, &[[1; 32], [2; 32]]);
        assert_eq!(s.handle(next).await, StoreResponse::Ok);
    }

    #[tokio::test]
    async fn ledger_loss_keeps_ownership_and_published_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let alice = SigningKey::from_bytes(&[1; 32]);
        let mallory = SigningKey::from_bytes(&[2; 32]);
        let pack = [4; 16];
        {
            let s = store(dir.path());
            assert_eq!(s.handle(blob(&alice, pack, [1; 32])).await, StoreResponse::Ok);
            assert_eq!(s.handle(manifest(&alice, pack, 1, &[[1; 32]])).await, StoreResponse::Ok);
        }
        for file in ["ledger.db", "ledger.db-wal", "ledger.db-shm"] {
            let _ = std::fs::remove_file(dir.path().join(file));
        }

        let s = store(dir.path());
        let takeover = manifest(&mallory, pack, 2, &[]);
        assert_eq!(s.handle(takeover).await, StoreResponse::Rejected(NotOwner));
        assert_eq!(s.handle(manifest(&alice, pack, 1, &[[1; 32]])).await, StoreResponse::Ok);
        assert_eq!(s.handle(blob(&alice, pack, [1; 32])).await, StoreResponse::Ok);
        s.sweep_before(now_secs() + 1).await;
        assert!(stored(dir.path(), pack, [1; 32]), "a replayed published blob expired");

        assert_eq!(s.handle(blob(&alice, pack, [2; 32])).await, StoreResponse::Ok);
        let next = manifest(&alice, pack, 2, &[[1; 32], [2; 32]]);
        assert_eq!(s.handle(next).await, StoreResponse::Ok);
        s.sweep_before(now_secs() + 1).await;
        assert!(stored(dir.path(), pack, [1; 32]) && stored(dir.path(), pack, [2; 32]));
        assert_eq!(s.ledger.lock().await.named_keys(&pack).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_sweep_retries_deletes_that_failed_and_spares_published_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let alice = SigningKey::from_bytes(&[1; 32]);
        let creator = alice.verifying_key().to_bytes();
        let (published, abandoned) = ([1; 16], [2; 16]);
        assert_eq!(s.handle(blob(&alice, published, [9; 32])).await, StoreResponse::Ok);
        let named = manifest(&alice, published, 1, &[[9; 32]]);
        assert_eq!(s.handle(named).await, StoreResponse::Ok);
        assert_eq!(s.handle(blob(&alice, abandoned, [7; 32])).await, StoreResponse::Ok);

        // A directory in its place makes the object undeletable, even for root.
        let stuck = dir.path().join("objects").join(blob_path(&abandoned, &[7; 32]));
        std::fs::remove_file(&stuck).unwrap();
        std::fs::create_dir(&stuck).unwrap();
        s.sweep_before(now_secs() + 1).await;
        assert_eq!(s.ledger.lock().await.unnamed_count(&abandoned).unwrap(), 1);
        assert_eq!(s.ledger.lock().await.packs_of(&creator).unwrap(), 2);

        std::fs::remove_dir(&stuck).unwrap();
        s.sweep_before(now_secs() + 1).await;
        assert_eq!(s.ledger.lock().await.unnamed_count(&abandoned).unwrap(), 0);
        assert_eq!(s.ledger.lock().await.packs_of(&creator).unwrap(), 1);
        assert!(stored(dir.path(), published, [9; 32]));
    }
}
