//! Bounded live activity with durable per-peer sent chunks. Serving is not a recipient receipt.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::params;

use crate::db::Stores;

use super::wire::Manifest;

const HISTORY: usize = 128;
const RATE_WINDOW: Duration = Duration::from_secs(3);
const GROUPS: usize = 96;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    file: [u8; 32],
    peer: [u8; 32],
    outgoing: bool,
}

struct Activity {
    total: u64,
    chunk_size: u32,
    chunks: usize,
    bits: Vec<u8>,
    bytes: u64,
    traffic: u64,
    samples: VecDeque<(Instant, u64)>,
    started: Instant,
    touched: Instant,
    active: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ChunkGroup {
    pub completed: u32,
    pub total: u32,
}

pub(crate) struct Snapshot {
    pub file: [u8; 32],
    pub peer: [u8; 32],
    pub outgoing: bool,
    pub active: bool,
    pub total: u64,
    pub bytes: u64,
    pub bytes_per_second: u64,
    pub elapsed_ms: u64,
    pub groups: Vec<ChunkGroup>,
}

#[derive(Default)]
pub(crate) struct Activities(Mutex<HashMap<Key, Activity>>);

pub(crate) struct ActivityGuard<'a> {
    activities: &'a Activities,
    key: Key,
    db: Option<&'a Stores>,
}

impl Activities {
    pub fn start(
        &self, file: [u8; 32], peer: [u8; 32], outgoing: bool, manifest: &Manifest,
        verified: Option<&[u8]>,
    ) -> ActivityGuard<'_> {
        let now = Instant::now();
        let key = Key { file, peer, outgoing };
        let mut rows = self.0.lock();
        if !rows.contains_key(&key)
            && rows.len() >= HISTORY
            && let Some(oldest) = rows
                .iter()
                .filter(|(_, row)| row.active == 0)
                .min_by_key(|(_, row)| row.touched)
                .map(|(key, _)| *key)
        {
            rows.remove(&oldest);
        }
        let row = rows.entry(key).or_insert_with(|| Activity {
            total: manifest.total_size,
            chunk_size: manifest.chunk_size,
            chunks: manifest.chunks.len(),
            bits: vec![0; manifest.chunks.len().div_ceil(8)],
            bytes: 0,
            traffic: 0,
            samples: VecDeque::from([(now, 0)]),
            started: now,
            touched: now,
            active: 0,
        });
        if row.active == 0
            && (now.duration_since(row.touched) > RATE_WINDOW || row.bytes == row.total)
        {
            row.samples = VecDeque::from([(now, row.traffic)]);
            row.started = now;
        }
        if let Some(bits) = verified {
            if outgoing {
                for (have, saved) in row.bits.iter_mut().zip(bits) {
                    *have |= saved;
                }
            } else {
                row.bits.copy_from_slice(bits);
            }
            row.bytes = verified_bytes(
                &row.bits,
                manifest.chunks.len(),
                manifest.chunk_size,
                manifest.total_size,
            );
        }
        row.active += 1;
        row.touched = now;
        ActivityGuard { activities: self, key, db: None }
    }

    pub fn start_serving<'a>(
        &'a self, db: &'a Stores, file: [u8; 32], peer: [u8; 32], manifest: &Manifest,
    ) -> anyhow::Result<ActivityGuard<'a>> {
        let mut guard = self.start(file, peer, true, manifest, None);
        {
            // Use the same lock order as record: another stream cannot advance the saved
            // bitmap between loading it and restoring live progress.
            let mut rows = self.0.lock();
            let row = rows.get_mut(&guard.key).expect("live serving activity");
            if let Some(saved) = saved(db, file, Some(peer))?.into_iter().next() {
                anyhow::ensure!(
                    saved.total == manifest.total_size && saved.chunk_size == manifest.chunk_size,
                    "stored sent-chunk progress does not match attachment"
                );
                row.bits = saved.bits;
            } else {
                row.bits.fill(0);
            }
            row.bytes = verified_bytes(&row.bits, row.chunks, row.chunk_size, row.total);
        }
        guard.db = Some(db);
        Ok(guard)
    }

    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.read(None)
    }

    pub fn details(&self, db: &Stores, file: [u8; 32]) -> anyhow::Result<Vec<Snapshot>> {
        let mut rows = self.read(Some(file));
        for saved in saved(db, file, None)? {
            let bytes = verified_bytes(&saved.bits, saved.chunks(), saved.chunk_size, saved.total);
            let groups = groups(&saved.bits, saved.chunks());
            if let Some(row) = rows.iter_mut().find(|row| row.outgoing && row.peer == saved.peer) {
                row.bytes = bytes;
                row.groups = groups;
            } else {
                rows.push(Snapshot {
                    file,
                    peer: saved.peer,
                    outgoing: true,
                    active: false,
                    total: saved.total,
                    bytes,
                    bytes_per_second: 0,
                    elapsed_ms: 0,
                    groups,
                });
            }
        }
        Ok(rows)
    }

    fn read(&self, file: Option<[u8; 32]>) -> Vec<Snapshot> {
        let now = Instant::now();
        self.0
            .lock()
            .iter()
            .filter(|(key, _)| file.is_none_or(|file| file == key.file))
            .map(|(key, row)| Snapshot {
                file: key.file,
                peer: key.peer,
                outgoing: key.outgoing,
                active: row.active > 0,
                total: row.total,
                bytes: row.bytes,
                bytes_per_second: row.rate(now),
                elapsed_ms: row.touched.duration_since(row.started).as_millis() as u64,
                groups: if file.is_some() { groups(&row.bits, row.chunks) } else { Vec::new() },
            })
            .collect()
    }
}

impl Activity {
    fn record(&mut self, index: u32, len: usize, now: Instant) {
        let index = index as usize;
        if index >= self.chunks {
            return;
        }
        let mask = 1 << (index % 8);
        if self.bits[index / 8] & mask == 0 {
            self.bits[index / 8] |= mask;
            let expected = (self.total - index as u64 * u64::from(self.chunk_size))
                .min(u64::from(self.chunk_size));
            self.bytes += expected;
        }
        self.traffic += len as u64;
        self.touched = now;
        if self
            .samples
            .back()
            .is_none_or(|(at, _)| now.duration_since(*at) >= Duration::from_millis(200))
        {
            self.samples.push_back((now, self.traffic));
        }
        while self.samples.len() > 1 && now.duration_since(self.samples[1].0) >= RATE_WINDOW {
            self.samples.pop_front();
        }
    }

    fn rate(&self, now: Instant) -> u64 {
        if self.active == 0 || now.duration_since(self.touched) >= RATE_WINDOW {
            return 0;
        }
        let Some(&(at, bytes)) = self.samples.front() else {
            return 0;
        };
        let seconds = now.duration_since(at).as_secs_f64();
        if seconds < 0.5 {
            return 0;
        }
        ((self.traffic - bytes) as f64 / seconds) as u64
    }
}

impl ActivityGuard<'_> {
    pub fn record(&self, index: u32, bytes: usize) -> anyhow::Result<()> {
        if let Some(row) = self.activities.0.lock().get_mut(&self.key) {
            if index as usize >= row.chunks {
                return Ok(());
            }
            if let Some(db) = self.db
                && row.bits[index as usize / 8] & (1 << (index % 8)) == 0
            {
                let mut bits = row.bits.clone();
                bits[index as usize / 8] |= 1 << (index % 8);
                let changed = db.transfers().lock().execute(
                    "INSERT INTO served (file_id, peer, total, chunk_size, sent)
                     SELECT ?1, ?2, ?3, ?4, ?5
                     WHERE EXISTS (SELECT 1 FROM retention WHERE file_id = ?1)
                        OR EXISTS (SELECT 1 FROM partials WHERE file_id = ?1)
                     ON CONFLICT(file_id, peer) DO UPDATE SET sent = excluded.sent",
                    params![self.key.file, self.key.peer, row.total, row.chunk_size, bits],
                )?;
                anyhow::ensure!(changed == 1, "attachment was forgotten while serving");
            }
            row.record(index, bytes, Instant::now());
        }
        Ok(())
    }
}

struct Saved {
    peer: [u8; 32],
    total: u64,
    chunk_size: u32,
    bits: Vec<u8>,
}

impl Saved {
    fn chunks(&self) -> usize {
        self.total.div_ceil(u64::from(self.chunk_size.max(1))) as usize
    }
}

fn saved(db: &Stores, file: [u8; 32], peer: Option<[u8; 32]>) -> anyhow::Result<Vec<Saved>> {
    let conn = db.transfers().lock();
    let mut stmt = conn.prepare(
        "SELECT peer, total, chunk_size, sent FROM served
         WHERE file_id = ?1 AND (?2 IS NULL OR peer = ?2)",
    )?;
    let rows = stmt
        .query_map(params![file, peer], |r| {
            Ok(Saved { peer: r.get(0)?, total: r.get(1)?, chunk_size: r.get(2)?, bits: r.get(3)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &rows {
        let chunks = row.chunks();
        anyhow::ensure!(
            row.chunk_size > 0
                && row.chunk_size as usize <= super::wire::CHUNK_SIZE
                && chunks <= 8 * 1024 * 1024 / 32
                && row.bits.len() == chunks.div_ceil(8)
                && (chunks.is_multiple_of(8)
                    || row.bits.last().is_none_or(|last| last >> (chunks % 8) == 0)),
            "invalid stored sent-chunk progress"
        );
    }
    Ok(rows)
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        if let Some(row) = self.activities.0.lock().get_mut(&self.key) {
            row.active -= 1;
            row.touched = Instant::now();
        }
    }
}

pub(crate) fn verified_bytes(bits: &[u8], chunks: usize, chunk_size: u32, total: u64) -> u64 {
    let count: u64 = bits.iter().map(|byte| u64::from(byte.count_ones())).sum();
    let last = chunks.saturating_sub(1);
    let short = chunks > 0 && bits.get(last / 8).is_some_and(|byte| byte & (1 << (last % 8)) != 0);
    (count * u64::from(chunk_size))
        .saturating_sub(if short {
            (chunks as u64 * u64::from(chunk_size)).saturating_sub(total)
        } else {
            0
        })
        .min(total)
}

pub(crate) fn groups(bits: &[u8], chunks: usize) -> Vec<ChunkGroup> {
    let count = chunks.min(GROUPS);
    (0..count)
        .map(|group| {
            let start = group * chunks / count;
            let end = (group + 1) * chunks / count;
            let have = (start..end)
                .filter(|index| {
                    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
                })
                .count();
            ChunkGroup { completed: have as u32, total: (end - start) as u32 }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_progress_preserves_every_chunk_and_the_short_last_group() {
        assert!(groups(&[], 0).is_empty());
        let mut bits = vec![0; 1153_usize.div_ceil(8)];
        bits[0] = 0b101;
        bits[144] = 1;
        let grouped = groups(&bits, 1153);
        assert_eq!(grouped.len(), GROUPS);
        assert_eq!(grouped[0], ChunkGroup { completed: 2, total: 12 });
        assert_eq!(grouped[95], ChunkGroup { completed: 1, total: 13 });
        assert_eq!(grouped.iter().map(|g| g.total).sum::<u32>(), 1153);
        assert_eq!(grouped.iter().map(|g| g.completed).sum::<u32>(), 3);
        assert!(groups(&[255], 3).iter().all(|g| g.completed == 1 && g.total == 1));
    }

    #[test]
    fn overlapping_streams_count_unique_progress_and_release_activity_independently() {
        let activities = Activities::default();
        let manifest = Manifest { total_size: 9, chunk_size: 4, chunks: vec![[0; 32]; 3] };
        let first = activities.start([1; 32], [2; 32], true, &manifest, None);
        let second = activities.start([1; 32], [2; 32], true, &manifest, None);
        first.record(0, 4).unwrap();
        second.record(0, 4).unwrap();
        second.record(2, 1).unwrap();
        drop(first);
        let row = activities.snapshots().remove(0);
        assert!(row.active);
        assert_eq!(row.bytes, 5);
        assert_eq!(activities.read(Some([1; 32]))[0].groups, groups(&[0b101], 3));
        drop(second);
        assert!(!activities.snapshots()[0].active);
    }

    #[test]
    fn resumed_bytes_do_not_inflate_rate_and_stalls_expire_it() {
        let activities = Activities::default();
        let manifest = Manifest { total_size: 9, chunk_size: 4, chunks: vec![[0; 32]; 3] };
        let guard = activities.start([1; 32], [2; 32], false, &manifest, Some(&[0b101]));
        let mut rows = activities.0.lock();
        let row = rows.values_mut().next().unwrap();
        let start = row.started;
        assert_eq!(row.bytes, 5);
        row.record(1, 4, start + Duration::from_secs(1));
        assert_eq!(row.bytes, 9);
        assert_eq!(row.rate(start + Duration::from_secs(1)), 4);
        assert_eq!(row.rate(start + Duration::from_secs(5)), 0);
        drop(rows);
        drop(guard);
    }

    #[test]
    fn serving_keeps_unique_progress_across_idle_gaps_and_repeated_downloads() {
        let activities = Activities::default();
        let manifest = Manifest { total_size: 9, chunk_size: 4, chunks: vec![[0; 32]; 3] };
        let first = activities.start([1; 32], [2; 32], true, &manifest, None);
        first.record(0, 4).unwrap();
        drop(first);
        activities.0.lock().values_mut().next().unwrap().touched -=
            RATE_WINDOW + Duration::from_secs(1);
        let resumed = activities.start([1; 32], [2; 32], true, &manifest, Some(&[0]));
        assert_eq!(activities.snapshots()[0].bytes, 4);
        resumed.record(0, 4).unwrap();
        resumed.record(1, 4).unwrap();
        resumed.record(2, 1).unwrap();
        drop(resumed);
        let repeated = activities.start([1; 32], [2; 32], true, &manifest, None);
        repeated.record(0, 4).unwrap();
        assert_eq!(activities.snapshots()[0].bytes, 9);
    }

    #[test]
    fn reopening_restores_per_peer_sent_chunks_without_old_rate_or_active_streams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().display().to_string();
        let manifest = Manifest { total_size: 9, chunk_size: 4, chunks: vec![[0; 32]; 3] };
        let file = manifest.file_id();
        {
            let db = Stores::open(path.clone(), path.clone());
            super::super::store::retention_put_tx(
                &db.transfers().lock(),
                &file,
                &dir.path().join("source").display().to_string(),
                9,
                4,
                &postcard::to_allocvec(&manifest).unwrap(),
                u64::MAX,
            )
            .unwrap();
            let activities = Activities::default();
            let first = activities.start_serving(&db, file, [2; 32], &manifest).unwrap();
            let second = activities.start_serving(&db, file, [2; 32], &manifest).unwrap();
            first.record(0, 4).unwrap();
            second.record(0, 4).unwrap();
            second.record(2, 1).unwrap();
            let other = activities.start_serving(&db, file, [3; 32], &manifest).unwrap();
            other.record(1, 4).unwrap();
            // A separate connection sees checkpoints before any guard/shutdown cleanup runs.
            let reopened = Stores::open(path.clone(), path.clone());
            let fresh = Activities::default();
            let rows = fresh.details(&reopened, file).unwrap();
            let row = rows.iter().find(|row| row.peer == [2; 32]).unwrap();
            assert_eq!((row.bytes, row.total, row.active, row.bytes_per_second), (5, 9, false, 0));
            assert_eq!(row.groups, groups(&[0b101], 3));
        }
        let db = Stores::open(path.clone(), path);
        let activities = Activities::default();
        let resumed = activities.start_serving(&db, file, [2; 32], &manifest).unwrap();
        assert_eq!(activities.snapshots()[0].bytes, 5);
        resumed.record(0, 4).unwrap();
        resumed.record(1, 4).unwrap();
        let rows = activities.details(&db, file).unwrap();
        assert_eq!(rows.iter().find(|row| row.peer == [2; 32]).unwrap().bytes, 9);
        assert_eq!(rows.iter().find(|row| row.peer == [3; 32]).unwrap().bytes, 4);
        assert_eq!(activities.snapshots()[0].bytes_per_second, 0, "no inherited rate");
    }

    #[tokio::test]
    async fn forgetting_a_file_removes_history_and_live_serving_cannot_recreate_it() {
        let dev = crate::test_support::transfer::device();
        let manifest = dev.retain(b"123456789", 4);
        let file = manifest.file_id();
        let activities = &dev.core.transfers.activity;
        let guard = activities.start_serving(&dev.core.db, file, [2; 32], &manifest).unwrap();
        guard.record(0, 4).unwrap();
        super::super::store::forget_file(dev.core, &file);
        assert!(guard.record(1, 4).is_err());
        assert!(Activities::default().details(&dev.core.db, file).unwrap().is_empty());
        dev.retain(b"123456789", 4);
        let resent = activities.start_serving(&dev.core.db, file, [2; 32], &manifest).unwrap();
        resent.record(2, 1).unwrap();
        let restored = Activities::default().details(&dev.core.db, file).unwrap();
        assert_eq!(restored[0].bytes, 1, "a new offer cannot inherit forgotten history");
        assert_eq!(restored[0].groups, groups(&[0b100], 3));
    }
}
