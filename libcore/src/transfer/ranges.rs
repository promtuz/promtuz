//! The single owner of a receiver file and its verified chunk ranges. It verifies, writes and syncs
//! each chunk, then commits the bitmap and the contiguous prefix in one transaction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use anyhow::Result;
use common::utils::now_secs;
use tokio_util::sync::CancellationToken;

use super::{store, wire};

use crate::db::Stores;
use crate::state::Core;

/// An 8 MiB manifest frame names at most this many chunk hashes.
const MAX_CHUNKS: usize = 8 * 1024 * 1024 / 32;

struct StopScanOnDrop(Option<CancellationToken>);

impl Drop for StopScanOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

async fn blocking_scan<T: Send + 'static>(
    c: &Core, lease: store::ReceiverLease,
    scan: impl FnOnce(&store::ReceiverLease, &CancellationToken) -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = tokio::select! {
        biased;
        _ = lease.cancel.cancelled() => return Err(store::Cancelled.into()),
        permit = c.transfers.recovery_scans.clone().acquire_owned() => permit?,
    };
    let stop = CancellationToken::new();
    let mut guard = StopScanOnDrop(Some(stop.clone()));
    let receiver = c.spawn_blocking(move || {
        let _permit = permit;
        // `lease` holds the registration until this closure exits, even if the JoinHandle drops.
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
    db: &'static Stores,
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
        || (!chunks.is_multiple_of(8) && bits.last().is_some_and(|last| last >> (chunks % 8) != 0))
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

    pub(crate) async fn open_async(
        c: &'static Core, file_id: [u8; 32], peer: [u8; 32], manifest: wire::Manifest,
        offered_size: u64, lease: &store::ReceiverLease,
    ) -> Result<Self> {
        blocking_scan(c, lease.clone(), move |lease, stop| {
            Self::open_inner(&c.db, file_id, peer, manifest, offered_size, lease, Some(stop))
        })
        .await
    }

    fn open_inner(
        db: &'static Stores, file_id: [u8; 32], peer: [u8; 32], manifest: wire::Manifest,
        offered_size: u64, lease: &store::ReceiverLease, stop: Option<&CancellationToken>,
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
        let previous = store::partial_get_tx(&db.transfers().lock(), &file_id);
        let (path, saved_file_exists) = match previous.as_ref() {
            Some(p) => match std::fs::metadata(&p.path) {
                Ok(_) => (p.path.clone(), true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (store::partial_path(db, &file_id), false)
                },
                Err(e) => return Err(e.into()),
            },
            None => (store::partial_path(db, &file_id), false),
        };
        let chunks = manifest.chunks.len();
        let mut verified = vec![0; chunks.div_ceil(8)];
        // A fresh manifest does not vouch for old metadata of a different file or chunk geometry.
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
            let saved = store::verified_bitmap_tx(&db.transfers().lock(), &file_id)?;
            match saved {
                Some(bits) => {
                    if validate_bitmap(&bits, chunks).is_ok() {
                        verified = bits;
                    }
                    // A corrupt bitmap trusts no chunks; publishing the empty one below keeps
                    // every retry from failing on the same blob.
                },
                None => {
                    for idx in 0..(p.have as usize).min(chunks) {
                        set_bit(&mut verified, idx, true);
                    }
                },
            }
        }
        let mut file = store::open_partial(db, lease, &path)?;
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
            updated_at: now_secs(),
        };
        // A promoted legacy prefix must be durable before its first bitmap publication, as in
        // commit().
        check_cancelled()?;
        file.sync_data()?;
        check_cancelled()?;
        let mut conn = db.transfers().lock();
        store::partial_put_verified_live_tx(&mut conn, &partial, &verified, lease)?;
        drop(conn);
        Ok(Self { db, file, manifest, partial, verified, cancel: lease.cancel.clone() })
    }

    pub(crate) fn prefix(&self) -> u32 {
        self.partial.have
    }

    pub(crate) fn contains(&self, index: u32) -> bool {
        (index as usize) < self.manifest.chunks.len() && bit(&self.verified, index as usize)
    }

    /// Sorted disjoint missing runs; `max_chunks` bounds the sum across all of them.
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

    /// Any arrival order; a valid duplicate is a no-op.
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
        // Adopt this state in memory only after the DB commit, so a DB failure cannot make the
        // next assignment skip uncommitted progress.
        let mut verified = self.verified.clone();
        set_bit(&mut verified, idx, true);
        // Resume from the previous prefix; rescanning it on each commit makes sequential pulls
        // quadratic.
        let have = self.partial.have
            + (self.partial.have as usize..self.manifest.chunks.len())
                .take_while(|idx| bit(&verified, *idx))
                .count() as u32;
        let updated_at = now_secs();
        store::partial_progress_verified_live_tx(
            &self.db.transfers().lock(),
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
        let updated_at = now_secs();
        store::partial_progress_verified_live_tx(
            &self.db.transfers().lock(),
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
    use super::*;
    use crate::test_support::transfer::Device;
    use crate::test_support::transfer::device;
    use crate::test_support::transfer::manifest;

    /// A 35-byte file in 8-byte chunks, its partial saved at a path the receiver must reuse.
    struct Fixture {
        dev:      Device,
        file_id:  [u8; 32],
        manifest: wire::Manifest,
        bytes:    Vec<u8>,
        path:     String,
        lease:    store::ReceiverLease,
    }

    fn fixture(seed: u8) -> Fixture {
        let dev = device();
        let bytes: Vec<u8> = (0..35).map(|n| seed.wrapping_add(n)).collect();
        let manifest = manifest(&bytes, 8);
        let file_id = manifest.file_id();
        let path = dev.dir.path().join("saved.part").display().to_string();
        std::fs::write(&path, []).unwrap();
        let lease = store::receiver_lease(dev.core, file_id);
        let partial = store::Partial {
            file_id,
            source_ipk: [seed; 32],
            total: manifest.total_size,
            chunk_size: manifest.chunk_size,
            manifest: Some(postcard::to_allocvec(&manifest).unwrap()),
            have: 0,
            state: store::HELD,
            path: path.clone(),
            updated_at: 0,
        };
        store::partial_put_live_tx(&dev.core.db.transfers().lock(), &partial, &lease).unwrap();
        Fixture { dev, file_id, manifest, bytes, path, lease }
    }

    impl Fixture {
        fn open(&self) -> Receiver {
            let (m, size) = (self.manifest.clone(), self.manifest.total_size);
            Receiver::open_inner(
                &self.dev.core.db,
                self.file_id,
                [0xe1; 32],
                m,
                size,
                &self.lease,
                None,
            )
            .unwrap()
        }

        fn chunk(&self, index: u32) -> &[u8] {
            let start = index as usize * 8;
            &self.bytes[start..(start + 8).min(self.bytes.len())]
        }

        fn bitmap(&self) -> Option<Vec<u8>> {
            store::verified_bitmap_tx(&self.dev.core.db.transfers().lock(), &self.file_id).unwrap()
        }

        fn forget(&self) {
            store::forget_file(self.dev.core, &self.file_id);
        }
    }

    /// Saves (`have`, `bitmap`, `saved` manifest) over `disk` and reopens it. Returns what the
    /// receiver still trusts, then fetches what it lacks and checks the result is byte-exact.
    fn reopen(
        seed: u8, disk: impl FnOnce(&mut Vec<u8>), have: u32, bitmap: Option<&[u8]>,
        saved: impl FnOnce(&mut wire::Manifest),
    ) -> (u32, Vec<ChunkRange>, u32) {
        let f = fixture(seed);
        let mut bytes = f.bytes.clone();
        disk(&mut bytes);
        std::fs::write(&f.path, bytes).unwrap();
        let mut manifest = f.manifest.clone();
        saved(&mut manifest);
        let mut p = f.dev.partial(&f.file_id).unwrap();
        p.have = have;
        p.manifest = Some(postcard::to_allocvec(&manifest).unwrap());
        let mut db = f.dev.core.db.transfers().lock();
        match bitmap {
            Some(bits) => store::partial_put_verified_live_tx(&mut db, &p, bits, &f.lease).unwrap(),
            None => store::partial_put_live_tx(&db, &p, &f.lease).unwrap(),
        }
        drop(db);
        let mut receiver = f.open();
        let trusted = (receiver.prefix(), receiver.missing(16, 16), f.dev.verified(&f.file_id));
        for range in receiver.missing(16, 16) {
            for index in range.start..range.end {
                receiver.commit(index, f.chunk(index), &f.lease).unwrap();
            }
        }
        receiver.finish(&f.lease).unwrap();
        let done = f.dev.partial(&f.file_id).unwrap();
        assert!(done.is_complete());
        assert_eq!(done.path, f.path, "the saved file is reused");
        assert_eq!(std::fs::read(&f.path).unwrap(), f.bytes);
        trusted
    }

    fn missing(runs: &[(u32, u32)]) -> Vec<ChunkRange> {
        runs.iter().map(|&(start, end)| ChunkRange { start, end }).collect()
    }

    #[tokio::test]
    async fn reopening_rechecks_every_saved_chunk_and_refetches_only_what_fails() {
        let corrupt = |b: &mut Vec<u8>, at: usize| b[at] ^= 0xff;
        let keep = |_: &mut wire::Manifest| {};
        assert_eq!(
            reopen(
                0x11,
                |b| {
                    b.truncate(24);
                    corrupt(b, 8)
                },
                3,
                None,
                keep
            ),
            (1, missing(&[(1, 2), (3, 5)]), 2),
            "an old prefix is only a candidate, and a hole keeps the chunks after it"
        );
        assert_eq!(
            reopen(
                0x31,
                |b| {
                    corrupt(b, 16);
                    b.truncate(34)
                },
                1,
                Some(&[0b10101]),
                keep
            ),
            (1, missing(&[(1, 5)]), 1),
            "a corrupt or truncated chunk loses its bit"
        );
        assert_eq!(
            reopen(0x51, |_| {}, 3, Some(&[0b111]), |m| m.chunks[0][0] ^= 1),
            (0, missing(&[(0, 5)]), 0),
            "a saved manifest that differs vouches for nothing"
        );
        assert_eq!(
            reopen(0x71, |_| {}, 3, Some(&[0, 0]), keep),
            (0, missing(&[(0, 5)]), 0),
            "a malformed bitmap trusts nothing and keeps the bytes"
        );
    }

    #[tokio::test]
    async fn forgetting_a_file_during_a_live_receiver_cannot_resurrect_its_row_or_bytes() {
        let f = fixture(0xb1);
        let mut receiver = f.open();
        receiver.commit(2, f.chunk(2), &f.lease).unwrap();
        f.forget();
        assert!(f.lease.cancel.is_cancelled());
        assert!(receiver.commit(0, f.chunk(0), &f.lease).unwrap_err().is::<store::Cancelled>());
        assert!(receiver.finish(&f.lease).unwrap_err().is::<store::Cancelled>());
        let state =
            super::super::pull::set_state(f.dev.core, &f.file_id, [1; 32], store::ACTIVE, &f.lease);
        assert!(state.unwrap_err().is::<store::Cancelled>());
        assert!(f.dev.partial(&f.file_id).is_none());
        assert!(!std::path::Path::new(&f.path).exists());

        let next = store::receiver_lease(f.dev.core, f.file_id);
        std::fs::write(&f.path, [0xef]).unwrap();
        let stale = receiver.commit(0, f.chunk(0), &next).unwrap_err();
        assert!(stale.is::<store::Cancelled>(), "the old receiver cannot touch a new download");
        assert_eq!(std::fs::read(&f.path).unwrap(), vec![0xef]);
    }

    #[tokio::test]
    async fn a_failed_write_or_progress_commit_leaves_progress_where_it_was() {
        let f = fixture(0xd1);
        let mut receiver = f.open();
        receiver.commit(2, f.chunk(2), &f.lease).unwrap();
        let reject = "CREATE TEMP TRIGGER reject BEFORE UPDATE OF verified ON partials
            BEGIN SELECT RAISE(ABORT, 'disk full'); END;";
        f.dev.core.db.transfers().lock().execute_batch(reject).unwrap();
        let error = receiver.commit(0, f.chunk(0), &f.lease).unwrap_err();
        f.dev.core.db.transfers().lock().execute_batch("DROP TRIGGER temp.reject").unwrap();
        assert!(error.is::<rusqlite::Error>());
        assert!(!receiver.contains(0));
        assert_eq!(receiver.prefix(), 0);
        assert_eq!(f.dev.partial(&f.file_id).unwrap().have, 0);
        assert_eq!(f.bitmap(), Some(vec![0b100]));
        receiver.commit(0, f.chunk(0), &f.lease).unwrap();
        assert_eq!((receiver.prefix(), f.bitmap()), (1, Some(vec![0b101])), "a retry commits once");

        receiver.file = File::open(&f.path).unwrap();
        assert!(receiver.commit(1, f.chunk(1), &f.lease).unwrap_err().is::<std::io::Error>());
        assert!(!receiver.contains(1));
        assert_eq!(f.bitmap(), Some(vec![0b101]));
    }
}
