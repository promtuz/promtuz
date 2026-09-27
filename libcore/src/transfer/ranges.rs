//! One owner of a receiver file and its durable verified chunk ranges.
//!
//! Network workers do not write shared files or publish progress themselves.
//! The owner verifies bytes, writes their exact offset, syncs them, then commits
//! the bitmap and the legacy contiguous prefix in one database transaction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use anyhow::Result;
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{store, wire};

/// The manifest frame is capped at 8 MiB, so it cannot name more chunk hashes
/// than this. A bitmap for that maximum is 32 KiB, independently of file size.
const MAX_CHUNKS: usize = 8 * 1024 * 1024 / 32;
// Recovery may read GiB of saved chunks. Keep it off Tokio's async workers
// and bound simultaneous disk scans independently of network download slots.
static RECOVERY_SCANS: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(2)));

struct StopScanOnDrop(Option<CancellationToken>);

impl Drop for StopScanOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

async fn blocking_scan<T: Send + 'static>(
    lease: store::ReceiverLease, slots: Arc<Semaphore>,
    scan: impl FnOnce(&store::ReceiverLease, &CancellationToken) -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = tokio::select! {
        biased;
        _ = lease.cancel.cancelled() => return Err(store::Cancelled.into()),
        permit = slots.acquire_owned() => permit?,
    };
    let stop = CancellationToken::new();
    let mut guard = StopScanOnDrop(Some(stop.clone()));
    let receiver = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // `lease` owns the registration until this closure actually exits,
        // even if its JoinHandle is dropped. Cancellation is checked between
        // chunks; an in-progress filesystem call must return first.
        scan(&lease, &stop)
    })
    .await??;
    guard.0 = None;
    Ok(receiver)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkRange {
    pub start: u32,
    pub end: u32,
}

pub(crate) struct Receiver {
    file: File,
    manifest: wire::Manifest,
    partial: store::Partial,
    verified: Vec<u8>,
    cancel: Arc<CancellationToken>,
}

fn invalid(message: &str) -> anyhow::Error {
    wire::InvalidFrame(message.into()).into()
}

fn validate_manifest(m: &wire::Manifest, file_id: &[u8; 32], offered_size: u64) -> Result<()> {
    if m.chunk_size == 0
        || m.chunk_size as usize > wire::CHUNK_SIZE
        || m.chunks.len() > MAX_CHUNKS
        || m.total_size != offered_size
        || m.chunks.len() as u64 != m.total_size.div_ceil(m.chunk_size as u64)
        || m.file_id() != *file_id
    {
        return Err(invalid("manifest does not match the attachment offer"));
    }
    Ok(())
}

fn validate_bitmap(bits: &[u8], chunks: usize) -> Result<()> {
    if chunks > MAX_CHUNKS
        || bits.len() != chunks.div_ceil(8)
        || (chunks % 8 != 0 && bits.last().is_some_and(|last| last >> (chunks % 8) != 0))
    {
        return Err(invalid("invalid stored verified chunk bitmap"));
    }
    Ok(())
}

fn bit(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

fn set_bit(bits: &mut [u8], index: usize, present: bool) {
    if present {
        bits[index / 8] |= 1 << (index % 8);
    } else {
        bits[index / 8] &= !(1 << (index % 8));
    }
}

fn prefix(bits: &[u8], chunks: usize) -> u32 {
    (0..chunks).take_while(|idx| bit(bits, *idx)).count() as u32
}

impl Receiver {
    /// Reopen the exact persisted path, then recheck candidate chunks. NULL
    /// range metadata means the old prefix is the candidate set. Neither a
    /// bitmap nor a prefix can make missing or corrupt bytes count as present.
    #[cfg(test)]
    pub(crate) fn open(
        file_id: [u8; 32], peer: [u8; 32], manifest: wire::Manifest, offered_size: u64,
        lease: &store::ReceiverLease,
    ) -> Result<Self> {
        Self::open_inner(file_id, peer, manifest, offered_size, lease, None)
    }

    pub(crate) async fn open_async(
        file_id: [u8; 32], peer: [u8; 32], manifest: wire::Manifest, offered_size: u64,
        lease: &store::ReceiverLease,
    ) -> Result<Self> {
        blocking_scan(lease.clone(), RECOVERY_SCANS.clone(), move |lease, stop| {
            Self::open_inner(file_id, peer, manifest, offered_size, lease, Some(stop))
        })
        .await
    }

    fn open_inner(
        file_id: [u8; 32], peer: [u8; 32], manifest: wire::Manifest, offered_size: u64,
        lease: &store::ReceiverLease, stop: Option<&CancellationToken>,
    ) -> Result<Self> {
        let check_cancelled = || {
            if lease.cancel.is_cancelled() || stop.is_some_and(CancellationToken::is_cancelled) {
                Err(anyhow::Error::from(store::Cancelled))
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
        validate_manifest(&manifest, &file_id, offered_size)?;
        let previous = store::partial_get(&file_id);
        let (path, saved_file_exists) = match previous.as_ref() {
            Some(p) => match std::fs::metadata(&p.path) {
                Ok(_) => (p.path.clone(), true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (store::partial_path(&file_id), false)
                },
                Err(e) => return Err(e.into()),
            },
            None => (store::partial_path(&file_id), false),
        };
        let chunks = manifest.chunks.len();
        let mut verified = vec![0; chunks.div_ceil(8)];
        // An authenticated fresh manifest does not retroactively validate old
        // metadata describing a different file or chunk geometry.
        if let Some(p) = previous.as_ref().filter(|p| {
            saved_file_exists
                && p.total == manifest.total_size
                && p.chunk_size == manifest.chunk_size
                && p.manifest
                    .as_deref()
                    .filter(|bytes| bytes.len() <= 8 * 1024 * 1024)
                    .and_then(|bytes| postcard::from_bytes::<wire::Manifest>(bytes).ok())
                    .is_some_and(|saved| {
                        validate_manifest(&saved, &file_id, offered_size).is_ok()
                            && saved == manifest
                    })
        }) {
            match store::verified_bitmap(&file_id)? {
                Some(bits) => {
                    if validate_bitmap(&bits, chunks).is_ok() {
                        verified = bits;
                    }
                    // Corrupt local metadata is repairable. Keep its file
                    // intact, but trust no chunks until they are downloaded
                    // and verified again. Publishing the empty bitmap below
                    // prevents every explicit retry failing on the same blob.
                },
                None => {
                    for idx in 0..(p.have as usize).min(chunks) {
                        set_bit(&mut verified, idx, true);
                    }
                },
            }
        }
        let mut file = store::open_partial(lease, &path)?;
        let mut buf = vec![0; manifest.chunk_size as usize];
        for idx in 0..chunks {
            if !bit(&verified, idx) {
                continue;
            }
            check_cancelled()?;
            let offset = idx as u64 * manifest.chunk_size as u64;
            let len = (manifest.total_size - offset).min(manifest.chunk_size as u64) as usize;
            file.seek(SeekFrom::Start(offset))?;
            let valid = match file.read_exact(&mut buf[..len]) {
                Ok(()) => *blake3::hash(&buf[..len]).as_bytes() == manifest.chunks[idx],
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
                Err(e) => return Err(e.into()),
            };
            if !valid {
                set_bit(&mut verified, idx, false);
            }
        }
        let partial = store::Partial {
            file_id,
            source_ipk: peer,
            total: manifest.total_size,
            chunk_size: manifest.chunk_size,
            manifest: Some(postcard::to_allocvec(&manifest)?),
            have: prefix(&verified, chunks),
            state: store::ACTIVE,
            path,
            updated_at: crate::utils::systime().as_secs(),
        };
        // Recovery may promote a legacy prefix into the range schema. Ensure
        // those checked bytes are durable before its first bitmap publication,
        // just as commit() does for freshly received chunks.
        check_cancelled()?;
        file.sync_data()?;
        check_cancelled()?;
        store::partial_put_verified_live(&partial, &verified, lease)?;
        Ok(Self { file, manifest, partial, verified, cancel: lease.cancel.clone() })
    }

    pub(crate) fn prefix(&self) -> u32 {
        self.partial.have
    }

    pub(crate) fn contains(&self, index: u32) -> bool {
        (index as usize) < self.manifest.chunks.len() && bit(&self.verified, index as usize)
    }

    /// Sorted disjoint missing runs. Both limits bound the whole assignment,
    /// including the sum of the chunks across several separated ranges.
    pub(crate) fn missing(&self, max_ranges: usize, max_chunks: u32) -> Vec<ChunkRange> {
        let mut ranges = Vec::new();
        let mut remaining = max_chunks;
        let mut idx = self.prefix();
        let total = self.manifest.chunks.len() as u32;
        while idx < total && ranges.len() < max_ranges && remaining > 0 {
            if self.contains(idx) {
                idx += 1;
                continue;
            }
            let start = idx;
            while idx < total && !self.contains(idx) && remaining > 0 {
                idx += 1;
                remaining -= 1;
            }
            ranges.push(ChunkRange { start, end: idx });
        }
        ranges
    }

    fn ensure_live(&self, lease: &store::ReceiverLease) -> Result<()> {
        if self.cancel.is_cancelled() || !Arc::ptr_eq(&self.cancel, &lease.cancel) {
            return Err(store::Cancelled.into());
        }
        Ok(())
    }

    /// Accept one chunk, independently of arrival order. Valid duplicates are
    /// idempotent for legacy suffix pulls: no extra progress or disk write.
    pub(crate) fn commit(
        &mut self, index: u32, bytes: &[u8], lease: &store::ReceiverLease,
    ) -> Result<()> {
        self.ensure_live(lease)?;
        let idx = index as usize;
        if idx >= self.manifest.chunks.len() {
            return Err(invalid("chunk index outside manifest"));
        }
        let offset = index as u64 * self.manifest.chunk_size as u64;
        let expected =
            (self.manifest.total_size - offset).min(self.manifest.chunk_size as u64) as usize;
        if bytes.len() != expected || *blake3::hash(bytes).as_bytes() != self.manifest.chunks[idx] {
            return Err(invalid("chunk does not match manifest"));
        }
        if self.contains(index) {
            return Ok(());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(bytes)?;
        self.file.flush()?;
        self.file.sync_data()?;
        // Only adopt this candidate state in memory after the DB transaction
        // commits. A DB failure may leave extra valid bytes on disk, but must
        // not make the next assignment skip uncommitted progress.
        let mut verified = self.verified.clone();
        set_bit(&mut verified, idx, true);
        // Begin at the previous first missing chunk: rescanning an ever-longer
        // complete prefix on each commit would make sequential pulls quadratic.
        let have = self.partial.have
            + (self.partial.have as usize..self.manifest.chunks.len())
                .take_while(|idx| bit(&verified, *idx))
                .count() as u32;
        let updated_at = crate::utils::systime().as_secs();
        store::partial_progress_verified_live(
            &self.partial.file_id,
            have,
            self.partial.state,
            updated_at,
            &verified,
            lease,
        )?;
        self.verified = verified;
        self.partial.have = have;
        self.partial.updated_at = updated_at;
        Ok(())
    }

    pub(crate) fn finish(&mut self, lease: &store::ReceiverLease) -> Result<()> {
        self.ensure_live(lease)?;
        if self.prefix() as usize != self.manifest.chunks.len() {
            return Err(invalid("attachment still has missing chunks"));
        }
        self.file.set_len(self.manifest.total_size)?;
        self.file.sync_data()?;
        let updated_at = crate::utils::systime().as_secs();
        store::partial_progress_verified_live(
            &self.partial.file_id,
            self.partial.have,
            store::DONE,
            updated_at,
            &self.verified,
            lease,
        )?;
        self.partial.state = store::DONE;
        self.partial.updated_at = updated_at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        file_id: [u8; 32],
        manifest: wire::Manifest,
        bytes: Vec<u8>,
        path: String,
        lease: store::ReceiverLease,
    }

    impl Fixture {
        fn new(seed: u8, len: usize) -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("promtuz-range-receiver-{}-{serial}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            // Respect the suite's isolated DB root. Setting one only when
            // absent also allows a focused ranges-only invocation.
            if std::env::var_os("PROMTUZ_DATA_DIR").is_none() {
                unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
            }
            let bytes: Vec<u8> = (0..len).map(|n| seed.wrapping_add(n as u8)).collect();
            let manifest = wire::Manifest {
                total_size: len as u64,
                chunk_size: 8,
                chunks: bytes.chunks(8).map(|c| *blake3::hash(c).as_bytes()).collect(),
            };
            let file_id = manifest.file_id();
            store::forget_partial(&file_id);
            let path = dir.join("persisted-noncanonical.part").to_str().unwrap().to_owned();
            std::fs::write(&path, []).unwrap();
            let partial = store::Partial {
                file_id,
                source_ipk: [seed; 32],
                total: manifest.total_size,
                chunk_size: manifest.chunk_size,
                manifest: Some(postcard::to_allocvec(&manifest).unwrap()),
                have: 0,
                state: store::HELD,
                path: path.clone(),
                updated_at: crate::utils::systime().as_secs(),
            };
            store::partial_put(&partial).unwrap();
            let lease = store::receiver_lease(file_id);
            Self { file_id, manifest, bytes, path, lease }
        }

        fn open(&self) -> Receiver {
            Receiver::open(
                self.file_id,
                [0xe1; 32],
                self.manifest.clone(),
                self.manifest.total_size,
                &self.lease,
            )
            .unwrap()
        }

        fn chunk(&self, index: usize) -> &[u8] {
            let start = index * self.manifest.chunk_size as usize;
            &self.bytes[start..(start + self.manifest.chunk_size as usize).min(self.bytes.len())]
        }

        fn bitmap(&self, bytes: &[u8]) {
            store::TRANSFERS_DB
                .lock()
                .execute(
                    "UPDATE partials SET verified=?2 WHERE file_id=?1",
                    rusqlite::params![self.file_id, bytes],
                )
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            store::forget_partial(&self.file_id);
            let _ = std::fs::remove_dir(std::path::Path::new(&self.path).parent().unwrap());
        }
    }

    #[test]
    fn legacy_prefix_is_rehashed_and_only_bad_chunks_are_lost() {
        let fixture = Fixture::new(0x11, 35);
        let mut bytes = fixture.bytes[..24].to_vec();
        bytes[8] ^= 0xff;
        std::fs::write(&fixture.path, bytes).unwrap();
        let mut partial = store::partial_get(&fixture.file_id).unwrap();
        partial.have = 3;
        store::partial_put(&partial).unwrap();
        assert!(store::verified_bitmap(&fixture.file_id).unwrap().is_none());

        let mut receiver = fixture.open();
        assert_eq!(receiver.prefix(), 1);
        assert!(receiver.contains(2), "a hole does not discard a later verified chunk");
        assert_eq!(
            receiver.missing(2, 2),
            vec![ChunkRange { start: 1, end: 2 }, ChunkRange { start: 3, end: 4 }]
        );
        assert!(receiver.missing(0, 100).is_empty());
        assert!(receiver.missing(3, 0).is_empty());
        assert_eq!(store::verified_count(&store::partial_get(&fixture.file_id).unwrap()), 2);
        assert!(receiver.finish(&fixture.lease).unwrap_err().is::<wire::InvalidFrame>());
        receiver.commit(4, fixture.chunk(4), &fixture.lease).unwrap();
        receiver.commit(1, fixture.chunk(1), &fixture.lease).unwrap();
        assert_eq!(receiver.prefix(), 3);
        receiver.commit(3, fixture.chunk(3), &fixture.lease).unwrap();
        receiver.finish(&fixture.lease).unwrap();
        let done = store::partial_get(&fixture.file_id).unwrap();
        assert!(done.is_complete());
        assert_eq!(done.have, 5, "legacy prefix reaches the partial trailing chunk");
        assert_eq!(done.path, fixture.path, "reuse the exact persisted file");
        assert_eq!(std::fs::read(&fixture.path).unwrap(), fixture.bytes);
    }

    #[test]
    fn sparse_reopen_clears_truncation_and_corruption_without_trusting_holes() {
        let fixture = Fixture::new(0x31, 35);
        let mut receiver = fixture.open();
        for index in [0, 2, 4] {
            receiver.commit(index, fixture.chunk(index as usize), &fixture.lease).unwrap();
        }
        assert_eq!(receiver.prefix(), 1);
        drop(receiver);
        let mut bytes = std::fs::read(&fixture.path).unwrap();
        bytes[16] ^= 0x80;
        bytes.truncate(34); // Last chunk is no longer complete.
        std::fs::write(&fixture.path, bytes).unwrap();

        let receiver = fixture.open();
        assert!(receiver.contains(0));
        assert!(!receiver.contains(1));
        assert!(!receiver.contains(2));
        assert!(!receiver.contains(4));
        assert_eq!(receiver.missing(16, 16), vec![ChunkRange { start: 1, end: 5 }]);
        assert_eq!(store::verified_count(&store::partial_get(&fixture.file_id).unwrap()), 1);
    }

    #[test]
    fn duplicates_do_not_inflate_progress_and_invalid_chunks_do_not_publish() {
        let fixture = Fixture::new(0x51, 19);
        let mut receiver = fixture.open();
        receiver.commit(2, fixture.chunk(2), &fixture.lease).unwrap();
        receiver.commit(2, fixture.chunk(2), &fixture.lease).unwrap();
        assert_eq!(receiver.prefix(), 0);
        assert_eq!(store::verified_count(&store::partial_get(&fixture.file_id).unwrap()), 1);
        for (index, bytes) in [(3, vec![]), (0, vec![0; 8]), (1, vec![0; 9]), (2, vec![0; 3])] {
            assert!(
                receiver
                    .commit(index, &bytes, &fixture.lease)
                    .unwrap_err()
                    .is::<wire::InvalidFrame>()
            );
        }
        assert_eq!(store::verified_bitmap(&fixture.file_id).unwrap().unwrap(), vec![0b100]);
        receiver.commit(1, fixture.chunk(1), &fixture.lease).unwrap();
        receiver.commit(0, fixture.chunk(0), &fixture.lease).unwrap();
        receiver.finish(&fixture.lease).unwrap();
        assert_eq!(std::fs::read(&fixture.path).unwrap(), fixture.bytes);
    }

    #[test]
    fn invalid_saved_manifest_cannot_promote_prefix_or_bitmap() {
        let fixture = Fixture::new(0x71, 19);
        std::fs::write(&fixture.path, &fixture.bytes).unwrap();
        let mut partial = store::partial_get(&fixture.file_id).unwrap();
        partial.have = 3;
        let mut invalid_manifest = fixture.manifest.clone();
        invalid_manifest.chunks[0][0] ^= 1;
        partial.manifest = Some(postcard::to_allocvec(&invalid_manifest).unwrap());
        store::partial_put(&partial).unwrap();
        fixture.bitmap(&[0b111]);
        let receiver = fixture.open();
        assert_eq!(receiver.prefix(), 0);
        assert_eq!(receiver.missing(2, 10), vec![ChunkRange { start: 0, end: 3 }]);
    }

    #[test]
    fn malformed_local_bitmaps_repair_without_deleting_bytes_or_inflating_progress() {
        let fixture = Fixture::new(0x91, 19);
        std::fs::write(&fixture.path, &fixture.bytes).unwrap();
        for bitmap in [vec![], vec![0, 0], vec![0b1000], vec![0xff; 32 * 1024 + 1]] {
            fixture.bitmap(&bitmap);
            let mut partial = store::partial_get(&fixture.file_id).unwrap();
            partial.have = 3;
            store::partial_put(&partial).unwrap();
            assert_eq!(store::verified_count(&partial), 0, "corrupt bitmap overrides old prefix");
            let receiver = fixture.open();
            assert_eq!(receiver.prefix(), 0);
            assert_eq!(store::verified_bitmap(&fixture.file_id).unwrap().unwrap(), vec![0]);
            assert_eq!(store::verified_count(&store::partial_get(&fixture.file_id).unwrap()), 0);
            assert_eq!(std::fs::read(&fixture.path).unwrap(), fixture.bytes);
        }
        let mut receiver = fixture.open();
        for index in 0..3 {
            receiver.commit(index, fixture.chunk(index as usize), &fixture.lease).unwrap();
        }
        receiver.finish(&fixture.lease).unwrap();
        assert!(store::partial_get(&fixture.file_id).unwrap().is_complete());

        // The separate, peer-provided manifest is still a hard protocol
        // failure. Repairing local metadata must not relax its validation.
        for chunk_size in [0, wire::CHUNK_SIZE as u32 + 1] {
            let mut manifest = fixture.manifest.clone();
            manifest.chunk_size = chunk_size;
            assert!(
                validate_manifest(&manifest, &fixture.file_id, 19)
                    .unwrap_err()
                    .is::<wire::InvalidFrame>()
            );
        }
        assert!(validate_manifest(&fixture.manifest, &fixture.file_id, 18).is_err());
        assert!(validate_bitmap(&vec![0; MAX_CHUNKS.div_ceil(8) + 1], MAX_CHUNKS + 1).is_err());
    }

    #[test]
    fn deletion_cancels_publication_and_does_not_modify_a_new_generation() {
        let fixture = Fixture::new(0xb1, 19);
        let mut receiver = fixture.open();
        receiver.commit(2, fixture.chunk(2), &fixture.lease).unwrap();
        store::forget_partial(&fixture.file_id);
        assert!(
            receiver
                .commit(0, fixture.chunk(0), &fixture.lease)
                .unwrap_err()
                .is::<store::Cancelled>()
        );
        assert!(receiver.finish(&fixture.lease).unwrap_err().is::<store::Cancelled>());
        assert!(store::partial_get(&fixture.file_id).is_none());
        assert!(!std::path::Path::new(&fixture.path).exists());

        let next = store::receiver_lease(fixture.file_id);
        std::fs::write(&fixture.path, [0xef]).unwrap();
        assert!(receiver.commit(0, fixture.chunk(0), &next).unwrap_err().is::<store::Cancelled>());
        assert_eq!(std::fs::read(&fixture.path).unwrap(), vec![0xef]);
        std::fs::remove_file(&fixture.path).unwrap();
    }

    #[test]
    fn failed_progress_transaction_retains_prior_database_and_memory_state() {
        let fixture = Fixture::new(0xd1, 19);
        let mut receiver = fixture.open();
        receiver.commit(2, fixture.chunk(2), &fixture.lease).unwrap();
        let trigger = format!("range_reject_{}", hex::encode(fixture.file_id));
        store::TRANSFERS_DB
            .lock()
            .execute_batch(&format!(
                "CREATE TEMP TRIGGER {trigger} BEFORE UPDATE OF verified ON partials
                 WHEN NEW.file_id=x'{}' BEGIN SELECT RAISE(ABORT, 'range progress fixture'); END;",
                hex::encode(fixture.file_id),
            ))
            .unwrap();
        let error = receiver.commit(0, fixture.chunk(0), &fixture.lease).unwrap_err();
        store::TRANSFERS_DB.lock().execute_batch(&format!("DROP TRIGGER {trigger}")).unwrap();
        assert!(error.is::<rusqlite::Error>());
        assert!(!receiver.contains(0));
        assert_eq!(receiver.prefix(), 0);
        assert_eq!(store::partial_get(&fixture.file_id).unwrap().have, 0);
        assert_eq!(store::verified_bitmap(&fixture.file_id).unwrap().unwrap(), vec![0b100]);
        assert_eq!(store::verified_count(&store::partial_get(&fixture.file_id).unwrap()), 1);
        // The synced bytes left by the failed transaction are harmless: the
        // assignment remains missing and an ordinary retry commits it once.
        receiver.commit(0, fixture.chunk(0), &fixture.lease).unwrap();
        assert_eq!(receiver.prefix(), 1);
    }

    #[test]
    fn local_write_failure_cannot_advance_verified_progress() {
        let fixture = Fixture::new(0xf1, 19);
        let mut receiver = fixture.open();
        receiver.file = File::open(&fixture.path).unwrap(); // Deliberately read-only fd.
        let error = receiver.commit(0, fixture.chunk(0), &fixture.lease).unwrap_err();
        assert!(error.is::<std::io::Error>());
        assert_eq!(receiver.prefix(), 0);
        assert!(!receiver.contains(0));
        assert_eq!(store::verified_bitmap(&fixture.file_id).unwrap().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn queued_scan_cancellation_never_starts_blocking_work() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let fixture = Fixture::new(0x21, 27);
        let lease = store::receiver_lease(fixture.file_id);
        let slots = Arc::new(Semaphore::new(1));
        let occupied = slots.clone().acquire_owned().await.unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let observed = started.clone();
        let worker_lease = lease.clone();
        let work = tokio::spawn(blocking_scan(worker_lease, slots, move |_, _| {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        }));
        tokio::task::yield_now().await;
        lease.cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), work).await.unwrap().unwrap();
        assert!(result.unwrap_err().is::<store::Cancelled>());
        assert!(!started.load(Ordering::SeqCst));
        drop(occupied);
        drop(lease);
        assert!(!store::receiver_registered(&fixture.file_id));
    }

    #[tokio::test]
    async fn aborted_scan_keeps_ownership_until_its_blocking_work_stops() {
        use std::time::Duration;

        let fixture = Fixture::new(0x41, 27);
        let lease = store::receiver_lease(fixture.file_id);
        let worker_lease = lease.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, cancelled) = tokio::sync::oneshot::channel();
        let file_id = fixture.file_id;
        let manifest = fixture.manifest.clone();
        let work = tokio::spawn(blocking_scan(
            worker_lease,
            Arc::new(Semaphore::new(1)),
            move |lease, stop| {
                let _ = started.send(());
                wait.recv_timeout(Duration::from_secs(5))?;
                let result =
                    Receiver::open_inner(file_id, [1; 32], manifest, 27, lease, Some(stop));
                let _ = finished.send(result.as_ref().is_err_and(|e| e.is::<store::Cancelled>()));
                result
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
        work.abort();
        assert!(work.await.err().unwrap().is_cancelled());
        // Dropping the async caller must stop only this scan. The caller's
        // generation remains valid until it or deletion explicitly cancels it.
        assert!(!lease.cancel.is_cancelled());
        drop(lease);
        assert!(store::receiver_registered(&file_id), "background owner protects recovery");
        release.send(()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), cancelled).await.unwrap().unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            while store::receiver_registered(&file_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            store::verified_bitmap(&file_id).unwrap().is_none(),
            "aborted scan published nothing"
        );
        assert_eq!(store::partial_get(&file_id).unwrap().state, store::HELD);
    }

    #[tokio::test]
    async fn replacement_generation_blocks_old_scan_publication_and_survives_its_drop() {
        use std::time::Duration;

        let fixture = Fixture::new(0x61, 27);
        let old = store::receiver_lease(fixture.file_id);
        let worker_lease = old.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let file_id = fixture.file_id;
        let manifest = fixture.manifest.clone();
        let work = tokio::spawn(blocking_scan(
            worker_lease,
            Arc::new(Semaphore::new(1)),
            move |lease, stop| {
                let _ = started.send(());
                wait.recv_timeout(Duration::from_secs(5))?;
                Receiver::open_inner(file_id, [1; 32], manifest, 27, lease, Some(stop))
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
        let new = store::receiver_lease(file_id);
        assert!(old.cancel.is_cancelled());
        drop(old);
        let mut partial = store::partial_get(&file_id).unwrap();
        partial.state = store::CONNECTING;
        store::partial_put_live(&partial, &new).unwrap();
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), work).await.unwrap().unwrap();
        assert!(result.err().unwrap().is::<store::Cancelled>());
        assert!(store::receiver_registered(&file_id), "old owner's drop preserves its successor");
        assert!(!new.cancel.is_cancelled());
        assert!(store::verified_bitmap(&file_id).unwrap().is_none());
        assert_eq!(store::partial_get(&file_id).unwrap().state, store::CONNECTING);
        drop(new);
        assert!(!store::receiver_registered(&file_id));
    }
}
