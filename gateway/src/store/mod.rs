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
use common::warn;
use tokio::sync::Mutex;

use crate::config::StoreConfig;
use crate::store::backend::Backend;
use crate::store::ledger::Ledger;
use crate::store::ledger::PackEntry;

/// Allows a replacement upload batch before abandoned blobs are cleaned up.
const UNNAMED_MAX: usize = 2 * PACK_MAX_STICKERS;
/// Retention for unpublished packs and unreferenced blobs.
const EXPIRY_SECS: u64 = 24 * 60 * 60;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct StickerStore {
    id: u16,
    backend: Backend,
    ledger: Mutex<Ledger>,
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
        Ok(Self { id: cfg.id, backend, ledger: Mutex::new(ledger) })
    }

    /// Reject invalid requests before accessing storage.
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
                ledger.claim(&pack, &env.creator.0, env.version, now())?;
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
                ledger.add_blob(&pack, &creator, &key, now())?;
            },
            StoreRequest::PutManifest { env, keys } => {
                let keys: Vec<[u8; 32]> = keys.iter().map(|k| k.0).collect();
                if env.version < version {
                    return Ok(StoreResponse::Rejected(StoreReject::StaleVersion));
                }
                let serialized = env.ser()?;
                if env.version == version
                    && self.backend.get(&manifest_path(&pack)).await?.as_deref()
                        != Some(serialized.as_slice())
                {
                    return Ok(StoreResponse::Rejected(StoreReject::StaleVersion));
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
                let unnamed = ledger.publish(&pack, &creator, env.version, &keys, now())?;
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
        self.sweep_before(now().saturating_sub(EXPIRY_SECS)).await;
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
    use std::path::PathBuf;

    use common::proto::sticker::StoreReject::*;
    use common::types::bytes::Bytes;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::config::BackendConfig;

    fn store(dir: &std::path::Path) -> StickerStore {
        StickerStore::from_config(&StoreConfig {
            id: 1,
            db: dir.join("ledger.db"),
            backend: BackendConfig::Fs { dir: dir.join("objects") },
        })
        .unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pz-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn manifest(key: &SigningKey, pack: [u8; 16], version: u32) -> StoreRequest {
        naming(key, pack, version, &[])
    }

    fn naming(key: &SigningKey, pack: [u8; 16], version: u32, keys: &[[u8; 32]]) -> StoreRequest {
        StoreRequest::PutManifest {
            env: ManifestEnvelope::signed(key, pack, 1, version, vec![version as u8; 8]),
            keys: keys.iter().map(|k| Bytes(*k)).collect(),
        }
    }

    fn put_blob(key: &SigningKey, pack: [u8; 16], k: [u8; 32]) -> StoreRequest {
        StoreRequest::signed_blob(key, pack, 1, k, vec![k[0]])
    }

    /// The upload order a client follows, then every refusal the gateway owes:
    /// a stranger, a rollback, a wrong store, and the pack quota.
    #[tokio::test]
    async fn publishes_a_pack_and_refuses_what_it_should() {
        let dir = tmp("publish");
        let s = store(&dir);
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let mallory = SigningKey::from_bytes(&[2u8; 32]);
        let pack = [7u8; 16];

        let blob = StoreRequest::signed_blob(&alice, pack, 1, [9; 32], vec![4, 4, 4]);
        assert_eq!(s.handle(blob.clone()).await, StoreResponse::Ok);
        assert_eq!(s.handle(blob).await, StoreResponse::Ok, "a replay is idempotent");
        assert_eq!(
            s.handle(naming(&alice, pack, 1, &[[8; 32]])).await,
            StoreResponse::Rejected(MissingBlob)
        );
        assert_eq!(s.handle(naming(&alice, pack, 1, &[[9; 32]])).await, StoreResponse::Ok);
        assert!(dir.join("objects").join(blob_path(&pack, &[9; 32])).exists());
        assert!(dir.join("objects").join(manifest_path(&pack)).exists());

        assert_eq!(s.handle(manifest(&alice, pack, 1)).await, StoreResponse::Rejected(PackFull));
        assert_eq!(s.handle(naming(&alice, pack, 2, &[[9; 32]])).await, StoreResponse::Ok);
        assert!(dir.join("objects").join(blob_path(&pack, &[9; 32])).exists(), "named blobs stay");
        assert_eq!(s.handle(manifest(&mallory, pack, 3)).await, StoreResponse::Rejected(NotOwner));
        assert_eq!(
            s.handle(StoreRequest::signed_blob(&mallory, pack, 1, [8; 32], vec![1])).await,
            StoreResponse::Rejected(NotOwner)
        );
        assert_eq!(
            s.handle(StoreRequest::signed_blob(&alice, pack, 2, [8; 32], vec![1])).await,
            StoreResponse::Rejected(WrongStore)
        );
        let mut forged = StoreRequest::signed_blob(&alice, pack, 1, [8; 32], vec![1]);
        if let StoreRequest::PutBlob { bytes, .. } = &mut forged {
            bytes.push(2);
        }
        assert_eq!(s.handle(forged).await, StoreResponse::Rejected(BadSignature));

        // Unnamed blobs are capped; a replay at the cap is still free; the
        // next manifest keeps what it names and drops the rest.
        let keys: Vec<[u8; 32]> = (0..UNNAMED_MAX as u16)
            .map(|i| {
                let mut k = [0xAAu8; 32];
                k[..2].copy_from_slice(&i.to_be_bytes());
                k
            })
            .collect();
        for k in &keys {
            assert_eq!(s.handle(put_blob(&alice, pack, *k)).await, StoreResponse::Ok);
        }
        assert_eq!(
            s.handle(put_blob(&alice, pack, [0xBB; 32])).await,
            StoreResponse::Rejected(PackFull),
            "one over the cap"
        );
        assert_eq!(
            s.handle(put_blob(&alice, pack, keys[0])).await,
            StoreResponse::Ok,
            "a replay is free"
        );
        let kept: Vec<[u8; 32]> = [[9; 32]].into_iter().chain(keys[..50].iter().copied()).collect();
        assert_eq!(s.handle(naming(&alice, pack, 3, &kept)).await, StoreResponse::Ok);
        let objects = dir.join("objects");
        assert!(objects.join(blob_path(&pack, &keys[49])).exists());
        assert!(!objects.join(blob_path(&pack, &keys[50])).exists(), "unnamed blobs are deleted");
        assert!(!objects.join(blob_path(&pack, &keys[199])).exists());
        let ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
        assert_eq!(ledger.unnamed_count(&pack).unwrap(), 0);
        assert_eq!(ledger.blob_count(&pack).unwrap(), 51);
        assert_eq!(
            s.handle(put_blob(&alice, pack, [0xBB; 32])).await,
            StoreResponse::Ok,
            "room again"
        );

        for i in 1..MAX_PACKS_PER_CREATOR as u8 {
            assert_eq!(s.handle(manifest(&alice, [i; 16], 1)).await, StoreResponse::Ok);
        }
        assert_eq!(
            s.handle(manifest(&alice, [0xEE; 16], 1)).await,
            StoreResponse::Rejected(QuotaExceeded),
            "a sixth pack is over quota"
        );
        assert_eq!(s.handle(manifest(&mallory, [0xEE; 16], 1)).await, StoreResponse::Ok);
    }

    /// A write the bucket refused claims nothing: the creator keeps their
    /// quota, and a manifest that failed to land leaves the version alone.
    #[tokio::test]
    async fn a_refused_write_claims_nothing() {
        let dir = tmp("refused");
        let s = store(&dir);
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let pack = [7u8; 16];
        let objects = dir.join("objects");
        // A file where the blob directory should be: the blob's write fails.
        let blob_dir = objects.join(blob_path(&pack, &[9; 32])).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(blob_dir.parent().unwrap()).unwrap();
        std::fs::write(&blob_dir, b"in the way").unwrap();
        let blob = StoreRequest::signed_blob(&alice, pack, 1, [9; 32], vec![4]);
        assert_eq!(s.handle(blob.clone()).await, StoreResponse::Rejected(Unavailable));
        let ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
        assert_eq!(ledger.pack_count().unwrap(), 0);
        assert_eq!(ledger.packs_of(&alice.verifying_key().to_bytes()).unwrap(), 0);

        // Out of the way, the same blob lands and claims the pack. A directory
        // where the manifest should go makes its rename fail: version stays.
        std::fs::remove_file(&blob_dir).unwrap();
        assert_eq!(s.handle(blob).await, StoreResponse::Ok);
        assert_eq!(ledger.get(&pack).unwrap().map(|e| e.version), Some(0));
        std::fs::create_dir_all(objects.join(manifest_path(&pack))).unwrap();
        assert_eq!(s.handle(manifest(&alice, pack, 1)).await, StoreResponse::Rejected(Unavailable));
        assert_eq!(ledger.get(&pack).unwrap().map(|e| e.version), Some(0));
    }

    /// What never got a manifest expires: its blobs go, the pack row goes,
    /// and the creator's quota slot comes back. Named blobs are untouched.
    #[tokio::test]
    async fn abandoned_uploads_expire() {
        let dir = tmp("expire");
        let s = store(&dir);
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let (kept, left) = ([1u8; 16], [2u8; 16]);
        assert_eq!(s.handle(put_blob(&alice, kept, [9; 32])).await, StoreResponse::Ok);
        assert_eq!(s.handle(naming(&alice, kept, 1, &[[9; 32]])).await, StoreResponse::Ok);
        assert_eq!(s.handle(put_blob(&alice, left, [7; 32])).await, StoreResponse::Ok);
        let ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
        assert_eq!(ledger.packs_of(&alice.verifying_key().to_bytes()).unwrap(), 2);

        s.sweep_before(now().saturating_sub(1)).await;
        assert_eq!(
            ledger.packs_of(&alice.verifying_key().to_bytes()).unwrap(),
            2,
            "nothing is old yet"
        );
        s.sweep_before(now() + 1).await;
        assert_eq!(
            ledger.packs_of(&alice.verifying_key().to_bytes()).unwrap(),
            1,
            "the manifest-less pack is gone"
        );
        assert!(ledger.get(&kept).unwrap().is_some());
        assert!(dir.join("objects").join(blob_path(&kept, &[9; 32])).exists());
        assert!(!dir.join("objects").join(blob_path(&left, &[7; 32])).exists());
        assert_eq!(
            s.handle(put_blob(&alice, left, [7; 32])).await,
            StoreResponse::Ok,
            "the id is free to claim again"
        );
    }

    /// A stored manifest that does not decode is not "no manifest": the pack
    /// stays unclaimable rather than going to the first writer.
    #[tokio::test]
    async fn an_undecodable_stored_manifest_refuses() {
        let dir = tmp("undecodable");
        let s = store(&dir);
        let pack = [7u8; 16];
        let path = dir.join("objects").join(manifest_path(&pack));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"\xff\xff\xff").unwrap();
        let mallory = SigningKey::from_bytes(&[2u8; 32]);
        assert_eq!(
            s.handle(manifest(&mallory, pack, 1)).await,
            StoreResponse::Rejected(Unavailable)
        );
        assert_eq!(Ledger::open(&dir.join("ledger.db")).unwrap().pack_count().unwrap(), 0);
    }

    /// Ownership survives losing the ledger: the stored manifest names the
    /// creator, so a stranger still can't take the pack over.
    #[tokio::test]
    async fn ownership_is_recovered_from_the_stored_manifest() {
        let dir = tmp("recover");
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let mallory = SigningKey::from_bytes(&[2u8; 32]);
        let pack = [7u8; 16];
        {
            let s = store(&dir);
            assert_eq!(s.handle(manifest(&alice, pack, 3)).await, StoreResponse::Ok);
        }
        for f in ["ledger.db", "ledger.db-wal", "ledger.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let s = store(&dir);
        assert_eq!(s.handle(manifest(&mallory, pack, 4)).await, StoreResponse::Rejected(NotOwner));
        assert_eq!(
            s.handle(manifest(&alice, pack, 3)).await,
            StoreResponse::Ok,
            "exact replay after recovery"
        );
        assert_eq!(s.handle(manifest(&alice, pack, 4)).await, StoreResponse::Ok);
    }

    #[tokio::test]
    async fn published_blobs_cannot_be_rotated_around_the_quota() {
        let dir = tmp("published-quota");
        let s = store(&dir);
        let alice = SigningKey::from_bytes(&[1; 32]);
        let pack = [4; 16];
        let keys: Vec<_> = (0..PACK_MAX_STICKERS as u8).map(|i| [i; 32]).collect();
        for key in &keys {
            assert_eq!(s.handle(put_blob(&alice, pack, *key)).await, StoreResponse::Ok);
        }
        let published = naming(&alice, pack, 1, &keys);
        assert_eq!(s.handle(published.clone()).await, StoreResponse::Ok);
        assert_eq!(s.handle(published).await, StoreResponse::Ok, "lost responses can be retried");
        assert_eq!(s.handle(put_blob(&alice, pack, [200; 32])).await, StoreResponse::Ok);
        assert_eq!(
            s.handle(naming(&alice, pack, 2, &[[200; 32]])).await,
            StoreResponse::Rejected(PackFull)
        );
        s.sweep_before(now() + 1).await;
        let ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
        assert_eq!(ledger.blob_count(&pack).unwrap(), PACK_MAX_STICKERS);
        assert!(dir.join("objects").join(blob_path(&pack, &keys[0])).exists());
        assert!(!dir.join("objects").join(blob_path(&pack, &[200; 32])).exists());
    }

    #[tokio::test]
    async fn appending_after_ledger_loss_recovers_existing_blob_references() {
        let dir = tmp("recover-blobs");
        let alice = SigningKey::from_bytes(&[1; 32]);
        let pack = [4; 16];
        {
            let s = store(&dir);
            assert_eq!(s.handle(put_blob(&alice, pack, [1; 32])).await, StoreResponse::Ok);
            assert_eq!(s.handle(naming(&alice, pack, 1, &[[1; 32]])).await, StoreResponse::Ok);
        }
        for f in ["ledger.db", "ledger.db-wal", "ledger.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let s = store(&dir);
        assert_eq!(s.handle(put_blob(&alice, pack, [1; 32])).await, StoreResponse::Ok);
        s.sweep_before(now() + 1).await;
        assert!(
            dir.join("objects").join(blob_path(&pack, &[1; 32])).exists(),
            "replaying a published blob after ledger loss must not make it expire"
        );
        assert_eq!(s.handle(put_blob(&alice, pack, [2; 32])).await, StoreResponse::Ok);
        assert_eq!(s.handle(naming(&alice, pack, 2, &[[1; 32], [2; 32]])).await, StoreResponse::Ok);
        s.sweep_before(now() + 1).await;
        assert_eq!(
            Ledger::open(&dir.join("ledger.db")).unwrap().named_keys(&pack).unwrap().len(),
            2
        );
        for key in [[1; 32], [2; 32]] {
            assert!(dir.join("objects").join(blob_path(&pack, &key)).exists());
        }
    }
}
