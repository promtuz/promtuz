//! Queue indexes and quotas live in Fjall and commit atomically with their messages.
//! Only the global byte counter stays in RAM. Existing queues are indexed once, in bounded
//! batches; an interrupted rebuild restarts safely without changing authoritative rows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::proto::client_rel::{DispatchP, Wake};
use common::proto::group_queue::Publication;
use common::proto::pack::{Packer, Unpacker};
use fjall::{Database, Keyspace, KeyspaceCreateOptions, UserKey, UserValue};
use parking_lot::Mutex;

use super::{MAX_QUEUED_PER_RECIPIENT, MessageKey, queued_dispatch};

pub const MAX_QUEUED_PER_SENDER: usize = 4_096;
pub const MAX_QUEUE_SENDER_BYTES: u64 = 8_000_000;
pub const MAX_QUEUE_RECIPIENT_BYTES: u64 = 32_000_000;
pub const MAX_QUEUE_BYTES: u64 = 6_000_000_000;
/// Admission pressure guard, not a filesystem quota: compaction can still grow the DB.
pub const MAX_QUEUE_DB_BYTES: u64 = 9_000_000_000;
pub const QUEUED_MESSAGE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Bound mutation memory and lock occupancy during drain, expiry, migration and admin clear.
const MUTATION_BATCH: usize = 256;
// Metadata is small and frequently updated; do not give each index the 64 MiB default
// memtable. These buffers and Fjall's shared block cache bound the working index memory.
const INDEX_MEMTABLE_BYTES: u64 = 4 * 1024 * 1024;
const USAGE_MEMTABLE_BYTES: u64 = 1024 * 1024;
const INDEX: &str = "queue_index_v1";
const USAGE: &str = "queue_usage_v1";
// Reader references: tag(1), payload key(49), reader index(2), expiry(8), logical bytes(8).
// Payload keys: queue kind(1), sender(32), dispatch id(16). Values: retention deadline(8),
// acceptance time(8), original audience count(8), digest(32), immutable publication.
// The usage index holds the remaining-reader count, so ACKs never rewrite the ciphertext.
// The last ACK replaces the publication with its 56-byte retry tombstone (audience count zero).
const READY: &[u8] = b"ready";
const TOTAL: &[u8] = b"total";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAdmission {
    Insert,
    AlreadyQueued,
    IdTakenByOther,
    Full(&'static str),
}

#[derive(Default)]
struct Usage {
    count: u64,
    bytes: u64,
}

impl Usage {
    fn read(ks: &Keyspace, key: &[u8]) -> fjall::Result<Self> {
        let Some(value) = ks.get(key)? else { return Ok(Self::default()) };
        if value.len() != 16 {
            return Err(invalid_metadata());
        }
        Ok(Self {
            count: u64::from_be_bytes(value[..8].try_into().unwrap()),
            bytes: u64::from_be_bytes(value[8..].try_into().unwrap()),
        })
    }
    fn encode(&self) -> [u8; 16] {
        let mut out = [0; 16];
        out[..8].copy_from_slice(&self.count.to_be_bytes());
        out[8..].copy_from_slice(&self.bytes.to_be_bytes());
        out
    }
}

struct Row {
    sender: Option<[u8; 32]>,
    bytes: u64,
    expires: u64,
}
impl Row {
    fn new(key: &MessageKey, bytes: u64, dispatch: Option<&DispatchP>) -> Self {
        let retention = u64::from_be_bytes(key.ts_be).saturating_add(QUEUED_MESSAGE_TTL_MS);
        let expires = dispatch
            .filter(|d| d.ttl_ms != 0 && d.wake != Wake::Call)
            .map_or(retention, |d| retention.min(d.accepted_at_ms.saturating_add(d.ttl_ms)));
        Self { sender: dispatch.map(|d| d.from.0), bytes, expires }
    }
    fn decode(value: &[u8]) -> fjall::Result<Self> {
        if value.len() != 49 || value[0] > 1 {
            return Err(invalid_metadata());
        }
        Ok(Self {
            sender: (value[0] == 1).then(|| value[1..33].try_into().unwrap()),
            bytes: u64::from_be_bytes(value[33..41].try_into().unwrap()),
            expires: u64::from_be_bytes(value[41..49].try_into().unwrap()),
        })
    }
    fn encode(&self) -> [u8; 49] {
        let mut out = [0; 49];
        if let Some(sender) = self.sender {
            out[0] = 1;
            out[1..33].copy_from_slice(&sender);
        }
        out[33..41].copy_from_slice(&self.bytes.to_be_bytes());
        out[41..49].copy_from_slice(&self.expires.to_be_bytes());
        out
    }
}

#[derive(Default)]
struct Budget {
    bytes: u64,
}

/// The raw keyspace is exposed only to storage maintenance for scans. All writes go through
/// this wrapper, including batched acknowledgements, so payloads and accounting cannot drift.
#[derive(Clone)]
pub struct Queue {
    pub(super) ks: Keyspace,
    db: Database,
    kind: u8,
    peers: [Keyspace; 4],
    index: Keyspace,
    usage: Keyspace,
    // ponytail: serialize quota changes; shard only if bounded batches still show contention.
    budget: Arc<Mutex<Budget>>,
}

impl Queue {
    pub fn iter(&self) -> fjall::Iter {
        self.ks.iter()
    }
    pub fn prefix(&self, prefix: impl AsRef<[u8]>) -> fjall::Iter {
        self.ks.prefix(prefix)
    }
    pub fn get(&self, key: impl AsRef<[u8]>) -> fjall::Result<Option<UserValue>> {
        let key = key.as_ref();
        let _guard = self.budget.lock();
        let Some(value) = self.ks.get(key)? else { return Ok(None) };
        let Some(reference) = reference(&value) else { return Ok(Some(value)) };
        let Some(body) = self.peers[3].get(reference)? else { return Err(invalid_metadata()) };
        let publication = Publication::deser(&body[56..]).map_err(|_| invalid_metadata())?;
        let n = u16::from_be_bytes(value[50..52].try_into().unwrap()) as usize;
        let recipient = publication.recipients.get(n).ok_or_else(invalid_metadata)?;
        let dispatch =
            publication.dispatch(recipient, number(&body, 8)).map_err(|_| invalid_metadata())?;
        Ok(Some(dispatch.ser().map_err(|_| invalid_metadata())?.into()))
    }
    pub(crate) fn len(&self) -> fjall::Result<usize> {
        self.ks.len()
    }

    /// A small page of keys, with no database iterator retained while the network awaits.
    pub fn key_page(
        &self, recipient: &[u8; 32], after: Option<UserKey>,
    ) -> fjall::Result<Vec<UserKey>> {
        use std::ops::Bound;
        let start = after
            .map_or_else(|| Bound::Included(UserKey::from(recipient.as_slice())), Bound::Excluded);
        let mut keys = Vec::new();
        for entry in self.ks.range::<UserKey, _>((start, Bound::Unbounded)).take(MUTATION_BATCH) {
            let key = entry.key()?;
            if !key.starts_with(recipient) {
                break;
            }
            keys.push(key);
        }
        Ok(keys)
    }

    pub fn keys_for(&self, recipient: &[u8; 32], id: &[u8; 16]) -> fjall::Result<Vec<MessageKey>> {
        self.index
            .range(index_range(self.kind, recipient, id))
            .map(|entry| {
                let key = entry.key()?;
                Ok(storage_key(&key)?.1)
            })
            .collect()
    }

    pub(super) fn open(
        db: &Database, messages: Keyspace, home: Keyspace, welcome: Keyspace,
    ) -> fjall::Result<[Self; 4]> {
        let index = db.keyspace(INDEX, || {
            KeyspaceCreateOptions::default().max_memtable_size(INDEX_MEMTABLE_BYTES)
        })?;
        let usage = db.keyspace(USAGE, || {
            KeyspaceCreateOptions::default().max_memtable_size(USAGE_MEMTABLE_BYTES)
        })?;
        let ready = usage.get(READY)?;
        let bytes = if ready.as_deref() == Some(&[1]) {
            let value = usage.get(TOTAL)?.ok_or_else(invalid_metadata)?;
            u64::from_be_bytes(value.as_ref().try_into().map_err(|_| invalid_metadata())?)
        } else {
            0
        };
        let budget = Arc::new(Mutex::new(Budget { bytes }));
        let payloads = db.keyspace("queue_payloads", KeyspaceCreateOptions::default)?;
        let peers = [messages, home, welcome, payloads];
        let queues = std::array::from_fn(|kind| Self {
            ks: peers[kind].clone(),
            db: db.clone(),
            kind: kind as u8,
            peers: peers.clone(),
            index: index.clone(),
            usage: usage.clone(),
            budget: budget.clone(),
        });
        if ready.as_deref() != Some(&[1]) {
            index.clear()?;
            usage.clear()?;
            let mut budget = budget.lock();
            for queue in &queues {
                let mut change = Change::new(queue, &budget);
                for (n, entry) in queue.ks.iter().enumerate() {
                    let (key, value) = entry.into_inner()?;
                    change.index_existing(queue.kind, &key, &value)?;
                    if (n + 1) % MUTATION_BATCH == 0 {
                        change.commit(&mut budget)?;
                        change = Change::new(queue, &budget);
                    }
                }
                change.commit(&mut budget)?;
            }
            let mut ready = db.batch().durability(None);
            ready.insert(&usage, TOTAL, budget.bytes.to_be_bytes());
            ready.insert(&usage, READY, [1]);
            ready.commit()?;
            db.persist(fjall::PersistMode::SyncAll)?;
        }
        Ok(queues)
    }

    /// `stored_at` is the local relay clock. A duplicate needs no serialization, quota writes
    /// or filesystem-size query, but the caller still waits for the persistence barrier.
    pub fn admit(&self, dispatch: &DispatchP, stored_at: u64) -> anyhow::Result<QueueAdmission> {
        let mut budget = self.budget.lock();
        if let Some(entry) =
            self.index.range(index_range(self.kind, &dispatch.to.0, &dispatch.id.0)).next()
        {
            let (_, value) = entry.into_inner()?;
            return Ok(if Row::decode(&value)?.sender == Some(dispatch.from.0) {
                QueueAdmission::AlreadyQueued
            } else {
                QueueAdmission::IdTakenByOther
            });
        }
        let value = dispatch.ser()?;
        let bytes = (MessageKey::SIZE + value.len()) as u64;
        let mut change = Change::new(self, &budget);
        if change.limit(&dispatch.to.0, &dispatch.from.0, bytes)?.is_some() {
            // Scan compact metadata only, and bound the number of writes per attempt.
            for entry in self.index.prefix(dispatch.to.0) {
                let (key, value) = entry.into_inner()?;
                if stored_at > Row::decode(&value)?.expires {
                    let (kind, key) = storage_key(&key)?;
                    change.delete(kind, key.as_bytes())?;
                    if change.removed >= MUTATION_BATCH {
                        break;
                    }
                }
            }
            change.commit(&mut budget)?;
            change = Change::new(self, &budget);
            if let Some(reason) = change.limit(&dispatch.to.0, &dispatch.from.0, bytes)? {
                return Ok(QueueAdmission::Full(reason));
            }
        }
        if self.disk_full(bytes)? {
            return Ok(QueueAdmission::Full("database bytes"));
        }
        let key = MessageKey::new(&dispatch.to.0, stored_at, &dispatch.id.0);
        change.insert(
            self.kind,
            key.as_bytes(),
            &value,
            Some(Row::new(&key, bytes, Some(dispatch))),
        )?;
        change.commit(&mut budget)?;
        Ok(QueueAdmission::Insert)
    }

    /// Atomically accepts a fixed audience. Each reader consumes the normal queue quota;
    /// the relay budget counts the ciphertext once, plus its small reader references.
    pub fn admit_group(
        &self, publication: &Publication, stored_at: u64,
    ) -> anyhow::Result<QueueAdmission> {
        anyhow::ensure!(
            !publication.recipients.is_empty()
                && publication.recipients.len() <= common::proto::group_queue::MAX_RECIPIENTS,
            "invalid queue audience"
        );
        let mut audience = HashSet::new();
        anyhow::ensure!(
            publication.recipients.iter().all(|r| audience.insert(r.to.0)),
            "duplicate reader"
        );
        let mut budget = self.budget.lock();
        let body_key =
            [vec![self.kind], publication.from.0.to_vec(), publication.id.0.to_vec()].concat();
        let encoded = publication.ser()?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &encoded);
        if let Some(body) = self.peers[3].get(&body_key)? {
            return Ok(if body.get(24..56) == Some(digest.as_ref()) {
                QueueAdmission::AlreadyQueued
            } else {
                QueueAdmission::IdTakenByOther
            });
        }
        let first = publication.dispatch(&publication.recipients[0], stored_at)?;
        let key = MessageKey::new(&first.to.0, stored_at, &first.id.0);
        let bytes = (MessageKey::SIZE + first.ser()?.len()) as u64;
        let row = Row::new(&key, bytes, Some(&first));
        let mut body = Vec::with_capacity(56 + encoded.len());
        body.extend_from_slice(&stored_at.saturating_add(QUEUED_MESSAGE_TTL_MS).to_be_bytes());
        body.extend_from_slice(&stored_at.to_be_bytes());
        body.extend_from_slice(&(publication.recipients.len() as u64).to_be_bytes());
        body.extend_from_slice(digest.as_ref());
        body.extend_from_slice(&encoded);
        let physical = (body_key.len()
            + body.len()
            + publication.recipients.len() * (MessageKey::SIZE + 68)) as u64;
        if budget.bytes.saturating_add(physical) > MAX_QUEUE_BYTES {
            return Ok(QueueAdmission::Full("relay bytes"));
        }
        if self.disk_full(physical)? {
            return Ok(QueueAdmission::Full("database bytes"));
        }
        let mut change = Change::new(self, &budget);
        for (n, recipient) in publication.recipients.iter().enumerate() {
            if self
                .index
                .range(index_range(self.kind, &recipient.to.0, &publication.id.0))
                .next()
                .is_some()
            {
                return Ok(QueueAdmission::IdTakenByOther);
            }
            if let Some(reason) =
                change.reader_limit(&recipient.to.0, &publication.from.0, bytes)?
            {
                return Ok(QueueAdmission::Full(reason));
            }
            let key = MessageKey::new(&recipient.to.0, stored_at, &publication.id.0);
            let mut value = Vec::with_capacity(68);
            value.push(255);
            value.extend_from_slice(&body_key);
            value.extend_from_slice(&(n as u16).to_be_bytes());
            value.extend_from_slice(&row.expires.to_be_bytes());
            value.extend_from_slice(&bytes.to_be_bytes());
            change.insert(
                self.kind,
                key.as_bytes(),
                &value,
                Some(Row { sender: row.sender, bytes, expires: row.expires }),
            )?;
        }
        change.usage(body_key.clone())?.count = publication.recipients.len() as u64;
        change.insert(3, &body_key, &body, None)?;
        change.commit(&mut budget)?;
        Ok(QueueAdmission::Insert)
    }

    pub fn group_accepted_at(&self, publication: &Publication) -> fjall::Result<Option<u64>> {
        let key =
            [vec![self.kind], publication.from.0.to_vec(), publication.id.0.to_vec()].concat();
        Ok(self.peers[3].get(key)?.map(|body| number(&body, 8)))
    }

    fn disk_full(&self, bytes: u64) -> fjall::Result<bool> {
        Ok(self.db.disk_space()?.saturating_add(self.db.write_buffer_size()).saturating_add(bytes)
            > MAX_QUEUE_DB_BYTES)
    }

    pub fn admit_welcome(&self, key: &[u8; 40], value: &[u8]) -> fjall::Result<QueueAdmission> {
        let mut budget = self.budget.lock();
        let old = self.ks.get(key)?;
        let old_bytes = old.as_ref().map_or(0, |v| (key.len() + v.len()) as u64);
        let bytes = (key.len() + value.len()) as u64;
        let growth = bytes.saturating_sub(old_bytes);
        if growth > 0 && budget.bytes.saturating_add(growth) > MAX_QUEUE_BYTES {
            return Ok(QueueAdmission::Full("relay bytes"));
        }
        if self.disk_full(bytes)? {
            return Ok(QueueAdmission::Full("database bytes"));
        }
        let mut change = Change::new(self, &budget);
        change.delete(self.kind, key)?;
        change.insert(self.kind, key, value, None)?;
        change.commit(&mut budget)?;
        Ok(QueueAdmission::Insert)
    }

    pub fn remove(&self, key: impl Into<UserKey>) -> fjall::Result<()> {
        self.remove_many(&[key.into()]).map(|_| ())
    }

    /// Bounded atomic batches include the index and quota changes. Repeated keys/ACKs are no-ops.
    pub fn remove_many(&self, keys: &[UserKey]) -> fjall::Result<usize> {
        let mut removed = 0;
        for chunk in keys.chunks(MUTATION_BATCH) {
            let mut budget = self.budget.lock();
            let mut change = Change::new(self, &budget);
            for key in chunk {
                change.delete(self.kind, key)?;
            }
            removed += change.removed;
            change.commit(&mut budget)?;
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub(super) fn remove_if(
        &self, key: impl Into<UserKey>, predicate: impl Fn(&[u8], &[u8]) -> bool,
    ) -> fjall::Result<()> {
        self.remove_many_if(&[key.into()], predicate).map(|_| ())
    }

    /// Check the current value under the lock: renewal after an expiry scan must survive.
    pub(super) fn remove_many_if(
        &self, keys: &[UserKey], predicate: impl Fn(&[u8], &[u8]) -> bool,
    ) -> fjall::Result<usize> {
        let mut removed = 0;
        for chunk in keys.chunks(MUTATION_BATCH) {
            let mut budget = self.budget.lock();
            let mut change = Change::new(self, &budget);
            for key in chunk {
                if let Some(value) = self.ks.get(key)?
                    && predicate(key, &value)
                {
                    change.delete(self.kind, key)?;
                }
            }
            removed += change.removed;
            change.commit(&mut budget)?;
        }
        Ok(removed)
    }

    pub fn clear(&self) -> fjall::Result<()> {
        loop {
            let keys = self
                .ks
                .iter()
                .take(MUTATION_BATCH)
                .map(|entry| entry.key())
                .collect::<fjall::Result<Vec<_>>>()?;
            if keys.is_empty() {
                return Ok(());
            }
            self.remove_many(&keys)?;
        }
    }

    #[cfg(test)]
    pub fn insert(
        &self, key: impl Into<UserKey>, value: impl Into<UserValue>,
    ) -> fjall::Result<()> {
        let (key, value) = (key.into(), value.into());
        let mut budget = self.budget.lock();
        let mut change = Change::new(self, &budget);
        change.delete(self.kind, &key)?;
        change.insert(self.kind, &key, &value, None)?;
        change.commit(&mut budget)
    }
}

/// Only the records touched by one bounded mutation are held in memory. No entry or sender map
/// survives its commit. The global counter is published in RAM only after the batch succeeds.
struct Change<'a> {
    queue: &'a Queue,
    batch: fjall::OwnedWriteBatch,
    usages: HashMap<Vec<u8>, Usage>,
    deleted: HashSet<(u8, Vec<u8>)>,
    bytes: u64,
    removed: usize,
}
impl<'a> Change<'a> {
    fn new(queue: &'a Queue, budget: &Budget) -> Self {
        Self {
            queue,
            batch: queue.db.batch().durability(None),
            usages: HashMap::new(),
            deleted: HashSet::new(),
            bytes: budget.bytes,
            removed: 0,
        }
    }
    fn limit(
        &mut self, recipient: &[u8; 32], sender: &[u8; 32], bytes: u64,
    ) -> fjall::Result<Option<&'static str>> {
        if self.bytes.saturating_add(bytes) > MAX_QUEUE_BYTES {
            return Ok(Some("relay bytes"));
        }
        self.reader_limit(recipient, sender, bytes)
    }

    fn reader_limit(
        &mut self, recipient: &[u8; 32], sender: &[u8; 32], bytes: u64,
    ) -> fjall::Result<Option<&'static str>> {
        let total = self.usage(recipient.to_vec())?;
        if total.count >= MAX_QUEUED_PER_RECIPIENT as u64 {
            return Ok(Some("recipient count"));
        }
        if total.bytes.saturating_add(bytes) > MAX_QUEUE_RECIPIENT_BYTES {
            return Ok(Some("recipient bytes"));
        }
        let sent = self.usage([recipient.as_slice(), sender.as_slice()].concat())?;
        if sent.count >= MAX_QUEUED_PER_SENDER as u64 {
            return Ok(Some("sender count"));
        }
        if sent.bytes.saturating_add(bytes) > MAX_QUEUE_SENDER_BYTES {
            return Ok(Some("sender bytes"));
        }
        Ok(None)
    }

    fn usage(&mut self, key: Vec<u8>) -> fjall::Result<&mut Usage> {
        match self.usages.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let usage = Usage::read(&self.queue.usage, entry.key())?;
                Ok(entry.insert(usage))
            },
        }
    }
    fn account(&mut self, key: &MessageKey, row: &Row, add: bool) -> fjall::Result<()> {
        let mut keys = vec![key.recipient.to_vec()];
        if let Some(sender) = row.sender {
            keys.push([key.recipient, sender].concat());
        }
        for key in keys {
            let usage = self.usage(key)?;
            if add {
                usage.count += 1;
                usage.bytes += row.bytes;
            } else {
                usage.count = usage.count.checked_sub(1).ok_or_else(invalid_metadata)?;
                usage.bytes = usage.bytes.checked_sub(row.bytes).ok_or_else(invalid_metadata)?;
            }
        }
        Ok(())
    }
    fn index_existing(&mut self, kind: u8, key: &[u8], value: &[u8]) -> fjall::Result<()> {
        let bytes = (key.len() + value.len()) as u64;
        self.bytes += bytes;
        if let Some(key) = MessageKey::parse(key) {
            let dispatch = queued_dispatch(&key.recipient, value);
            let row = if let Some(body_key) = reference(value) {
                self.usage(body_key.to_vec())?.count += 1;
                Row {
                    sender: Some(body_key[1..33].try_into().unwrap()),
                    bytes: number(value, 60),
                    expires: number(value, 52),
                }
            } else {
                Row::new(&key, bytes, dispatch.as_ref())
            };
            self.batch.insert(&self.queue.index, index_key(kind, &key), row.encode());
            self.account(&key, &row, true)?;
        }
        Ok(())
    }
    fn insert(
        &mut self, kind: u8, key: &[u8], value: &[u8], row: Option<Row>,
    ) -> fjall::Result<()> {
        self.batch.insert(&self.queue.peers[kind as usize], key, value);
        if let Some(row) = row {
            let key = MessageKey::parse(key).ok_or_else(invalid_metadata)?;
            self.bytes += (key.as_bytes().len() + value.len()) as u64;
            self.batch.insert(&self.queue.index, index_key(kind, &key), row.encode());
            self.account(&key, &row, true)?;
        } else {
            self.index_existing(kind, key, value)?;
        }
        Ok(())
    }
    fn delete(&mut self, kind: u8, key: &[u8]) -> fjall::Result<()> {
        if !self.deleted.insert((kind, key.to_vec())) {
            return Ok(());
        }
        let bytes = if let Some(message) = MessageKey::parse(key) {
            let index = index_key(kind, &message);
            let Some(value) = self.queue.index.get(index)? else { return Ok(()) };
            let row = Row::decode(&value)?;
            self.batch.remove(&self.queue.index, index);
            self.account(&message, &row, false)?;
            let value = self.queue.peers[kind as usize].get(key)?.ok_or_else(invalid_metadata)?;
            if let Some(body_key) = reference(&value) {
                let usage = self.usage(body_key.to_vec())?;
                usage.count = usage.count.checked_sub(1).ok_or_else(invalid_metadata)?;
                if usage.count == 0 {
                    let body = self.queue.peers[3].get(body_key)?.ok_or_else(invalid_metadata)?;
                    self.bytes = self
                        .bytes
                        .checked_sub((body.len() - 56) as u64)
                        .ok_or_else(invalid_metadata)?;
                    let mut tombstone = body[..56].to_vec();
                    tombstone[16..24].fill(0);
                    self.batch.insert(&self.queue.peers[3], body_key, tombstone);
                }
            }
            (key.len() + value.len()) as u64
        } else {
            let Some(value) = self.queue.peers[kind as usize].get(key)? else { return Ok(()) };
            (key.len() + value.len()) as u64
        };
        self.bytes = self.bytes.checked_sub(bytes).ok_or_else(invalid_metadata)?;
        self.batch.remove(&self.queue.peers[kind as usize], key);
        self.removed += 1;
        Ok(())
    }
    fn commit(mut self, budget: &mut Budget) -> fjall::Result<()> {
        if self.batch.is_empty() && self.bytes == budget.bytes {
            return Ok(());
        }
        for (key, usage) in self.usages {
            if usage.count == 0 {
                self.batch.remove(&self.queue.usage, key);
            } else {
                self.batch.insert(&self.queue.usage, key, usage.encode());
            }
        }
        self.batch.insert(&self.queue.usage, TOTAL, self.bytes.to_be_bytes());
        self.batch.commit()?;
        budget.bytes = self.bytes;
        Ok(())
    }
}

fn number(value: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(value[offset..offset + 8].try_into().unwrap())
}
fn reference(value: &[u8]) -> Option<&[u8]> {
    (value.len() == 68 && value[0] == 255).then(|| &value[1..50])
}
pub(super) fn reference_expired(value: &[u8], now: u64) -> bool {
    reference(value).is_some() && now > number(value, 52)
}
pub(super) fn payload_expired(_: &[u8], value: &[u8], now: u64) -> bool {
    value.len() >= 56 && number(value, 16) == 0 && now > number(value, 0)
}

fn invalid_metadata() -> fjall::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid queue accounting metadata").into()
}
fn index_key(kind: u8, key: &MessageKey) -> [u8; 57] {
    let mut index = [0; 57];
    index[..32].copy_from_slice(&key.recipient);
    index[32] = kind;
    index[33..49].copy_from_slice(&key.id);
    index[49..].copy_from_slice(&key.ts_be);
    index
}
fn index_range(
    kind: u8, recipient: &[u8; 32], id: &[u8; 16],
) -> std::ops::RangeInclusive<[u8; 57]> {
    index_key(kind, &MessageKey::new(recipient, 0, id))
        ..=index_key(kind, &MessageKey::new(recipient, u64::MAX, id))
}
fn storage_key(index: &[u8]) -> fjall::Result<(u8, MessageKey)> {
    if index.len() != 57 || index[32] > 2 {
        return Err(invalid_metadata());
    }
    Ok((
        index[32],
        MessageKey {
            recipient: index[..32].try_into().unwrap(),
            id: index[33..49].try_into().unwrap(),
            ts_be: index[49..].try_into().unwrap(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::super::db::Store;
    use super::*;
    use common::proto::client_rel::Wake;
    use common::utils::now_ms;

    #[test]
    fn legacy_rows_and_an_interrupted_index_build_recover_without_double_accounting() {
        let dir = tempfile::tempdir().unwrap();
        let row = dispatch(1, 2, 3, 200);
        let a = MessageKey::new(&row.to.0, row.accepted_at_ms, &row.id.0);
        let b = MessageKey::new(&row.to.0, row.accepted_at_ms + 1, &row.id.0);
        // Simulate an old store and an interrupted upgrade; no READY marker was committed.
        {
            let db = Database::builder(dir.path()).open().unwrap();
            let messages =
                db.keyspace(super::super::db::KS_MESSAGES, KeyspaceCreateOptions::default).unwrap();
            let legacy =
                (row.id, row.from, row.payload.clone(), row.sig, row.accepted_at_ms).ser().unwrap();
            messages.insert(a.as_bytes(), &legacy).unwrap();
            messages.insert(b.as_bytes(), &legacy).unwrap();
            let usage = db.keyspace(USAGE, KeyspaceCreateOptions::default).unwrap();
            usage.insert(TOTAL, u64::MAX.to_be_bytes()).unwrap();
            let index = db.keyspace(INDEX, KeyspaceCreateOptions::default).unwrap();
            index.insert(b"incomplete", b"garbage").unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let bytes = store
            .messages
            .iter()
            .map(|entry| {
                let (key, value) = entry.into_inner().unwrap();
                (key.len() + value.len()) as u64
            })
            .sum::<u64>();
        assert_eq!(store.messages.budget.lock().bytes, bytes);
        assert_eq!(store.messages.keys_for(&row.to.0, &row.id.0).unwrap().len(), 2);
        assert_eq!(
            store.messages.admit(&row, row.accepted_at_ms + 10).unwrap(),
            QueueAdmission::AlreadyQueued
        );
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.messages.budget.lock().bytes, bytes);
        // Duplicate ACK keys across a batch must not release quota twice.
        assert_eq!(
            store
                .messages
                .remove_many(&[a.as_bytes().into(), b.as_bytes().into(), a.as_bytes().into()])
                .unwrap(),
            2
        );
        assert_eq!(store.messages.budget.lock().bytes, 0);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.messages.len().unwrap(), 0);
        assert_eq!(store.messages.budget.lock().bytes, 0);
    }

    #[test]
    fn a_failed_batch_keeps_rows_indexes_and_all_quotas_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let a = dispatch(1, 2, 3, 100);
        let b = dispatch(2, 4, 5, 100);
        for row in [&a, &b] {
            store.messages.admit(row, row.accepted_at_ms).unwrap();
        }
        let keys: Vec<UserKey> = [&a, &b]
            .map(|r| MessageKey::new(&r.to.0, r.accepted_at_ms, &r.id.0).as_bytes().into())
            .into();
        let before = store.messages.budget.lock().bytes;
        // Fail while assembling the second deletion, after the first one has been staged.
        let sender_key = [b.to.0, b.from.0].concat();
        let saved = store.messages.usage.get(&sender_key).unwrap().unwrap();
        store.messages.usage.insert(&sender_key, Usage::default().encode()).unwrap();
        assert!(store.messages.remove_many(&keys).is_err());
        assert_eq!(store.messages.len().unwrap(), 2);
        assert_eq!(store.messages.index.len().unwrap(), 2);
        assert_eq!(store.messages.budget.lock().bytes, before);
        assert_eq!(Usage::read(&store.messages.usage, &a.to.0).unwrap().count, 1);
        store.messages.usage.insert(&sender_key, saved).unwrap();
        assert_eq!(store.messages.remove_many(&keys).unwrap(), 2);
        assert_eq!(store.messages.budget.lock().bytes, 0);
    }

    #[test]
    fn durable_queue_batches_recover_after_process_exit_without_drop() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "storage::queue::tests::queue_crash_child"])
            .env("PROMTUZ_QUEUE_CRASH_DB", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.messages.len().unwrap(), 3);
        assert_eq!(store.messages.index.len().unwrap(), 3);
        let bytes: u64 = [&store.messages, &store.payloads]
            .into_iter()
            .flat_map(|q| q.iter())
            .map(|r| {
                let (k, v) = r.into_inner().unwrap();
                (k.len() + v.len()) as u64
            })
            .sum();
        assert_eq!(store.messages.budget.lock().bytes, bytes);
        for row in store.messages.iter() {
            let key = row.key().unwrap();
            assert!(store.messages.get(key).unwrap().is_some());
        }
        store.clear_all().unwrap();
        assert_eq!(store.messages.budget.lock().bytes, 0);
    }

    #[test]
    fn queue_crash_child() {
        let Some(path) = std::env::var_os("PROMTUZ_QUEUE_CRASH_DB") else { return };
        let store = Store::open(path).unwrap();
        let a = dispatch(1, 2, 3, 100);
        let b = dispatch(2, 2, 3, 100);
        for row in [&a, &b] {
            store.messages.admit(row, row.accepted_at_ms).unwrap();
        }
        store
            .messages
            .remove(MessageKey::new(&a.to.0, a.accepted_at_ms, &a.id.0).as_bytes())
            .unwrap();
        let group = publication();
        let at = now_ms();
        store.messages.admit_group(&group, at).unwrap();
        store
            .messages
            .remove(MessageKey::new(&group.recipients[0].to.0, at, &group.id.0).as_bytes())
            .unwrap();
        store.messages.db.persist(fjall::PersistMode::SyncAll).unwrap();
        std::process::exit(0);
    }

    fn publication() -> Publication {
        Publication {
            from: [1; 32].into(),
            id: [2; 16].into(),
            group: [3; 32].into(),
            branch: [4; 32].into(),
            epoch: 7,
            message: vec![5; 128_000].into(),
            proof: None,
            wake: Wake::Message,
            ttl_ms: 1000,
            recipients: [6, 7, 8]
                .map(|n| common::proto::group_queue::Recipient {
                    to: [n; 32].into(),
                    envelope_sig: [n; 64].into(),
                    dispatch_sig: [n; 64].into(),
                })
                .into(),
        }
    }

    #[test]
    fn shared_payload_survives_partial_ack_restart_and_retry_without_resurrecting_readers() {
        let dir = tempfile::tempdir().unwrap();
        let group = publication();
        let at = now_ms();
        let store = Store::open_empty(dir.path());
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| store.messages.admit_group(&group, at).unwrap()))
                .collect();
            assert_eq!(
                jobs.into_iter()
                    .map(|j| j.join().unwrap())
                    .filter(|a| *a == QueueAdmission::Insert)
                    .count(),
                1
            );
        });
        assert_eq!(store.messages.len().unwrap(), 3);
        let physical = store.messages.budget.lock().bytes;
        assert!(physical < 130_000, "only one ciphertext is stored: {physical}");
        let keys: Vec<UserKey> = group
            .recipients
            .iter()
            .map(|r| MessageKey::new(&r.to.0, at, &group.id.0).as_bytes().into())
            .collect();
        for (reader, key) in group.recipients.iter().zip(&keys) {
            let value = store.messages.get(key).unwrap().unwrap();
            assert_eq!(DispatchP::deser(&value).unwrap(), group.dispatch(reader, at).unwrap());
            assert!(Usage::read(&store.messages.usage, &reader.to.0).unwrap().bytes > 128_000);
        }
        let (_, body) = store.payloads.iter().next().unwrap().into_inner().unwrap();
        store.messages.remove(keys[0].clone()).unwrap();
        assert_eq!(
            store.payloads.iter().next().unwrap().value().unwrap(),
            body,
            "partial ACK does not rewrite ciphertext"
        );
        // Rebuild the existing index after an interrupted rebuild, including reader counts.
        store.messages.usage.remove(READY).unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.messages.admit_group(&group, at + 10).unwrap(),
            QueueAdmission::AlreadyQueued
        );
        assert_eq!(store.messages.group_accepted_at(&group).unwrap(), Some(at));
        assert!(store.messages.get(&keys[0]).unwrap().is_none());
        assert_eq!(store.messages.len().unwrap(), 2);
        // TTL expiry of outstanding references uses the ordinary queue deletion path.
        store.messages.remove_many_if(&keys, |_, v| reference_expired(v, at + 1001)).unwrap();
        assert_eq!(store.messages.len().unwrap(), 0);
        assert_eq!(store.messages.budget.lock().bytes, 105, "only the retry tombstone remains");
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.messages.admit_group(&group, at + 20).unwrap(),
            QueueAdmission::AlreadyQueued
        );
        assert_eq!(store.messages.len().unwrap(), 0);
        let mut changed = group.clone();
        changed.message.0[0] ^= 1;
        assert_eq!(
            store.messages.admit_group(&changed, at).unwrap(),
            QueueAdmission::IdTakenByOther
        );
        let tombstones: Vec<_> = store.payloads.iter().map(|r| r.key().unwrap()).collect();
        store
            .payloads
            .remove_many_if(&tombstones, |k, v| {
                payload_expired(k, v, at + QUEUED_MESSAGE_TTL_MS + 1)
            })
            .unwrap();
        assert_eq!(store.messages.budget.lock().bytes, 0);
    }

    #[test]
    fn group_admission_and_reference_deletion_are_atomic_with_quotas() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let group = publication();
        let last = group.recipients.last().unwrap().to.0;
        store
            .messages
            .usage
            .insert(last, Usage { count: MAX_QUEUED_PER_RECIPIENT as u64, bytes: 0 }.encode())
            .unwrap();
        assert_eq!(
            store.messages.admit_group(&group, 50).unwrap(),
            QueueAdmission::Full("recipient count")
        );
        assert_eq!(store.messages.len().unwrap(), 0);
        assert_eq!(store.payloads.len().unwrap(), 0);
        assert_eq!(store.messages.budget.lock().bytes, 0);
        store.messages.usage.remove(last).unwrap();
        assert_eq!(store.messages.admit_group(&group, 50).unwrap(), QueueAdmission::Insert);
        let baseline = store.messages.budget.lock().bytes;
        let keys: Vec<UserKey> = group
            .recipients
            .iter()
            .map(|r| MessageKey::new(&r.to.0, 50, &group.id.0).as_bytes().into())
            .collect();
        let sender = [last, group.from.0].concat();
        let saved = store.messages.usage.get(&sender).unwrap().unwrap();
        store.messages.usage.insert(&sender, Usage::default().encode()).unwrap();
        assert!(store.messages.remove_many(&keys).is_err());
        assert_eq!(store.messages.budget.lock().bytes, baseline);
        assert_eq!(store.messages.len().unwrap(), 3);
        assert!(store.messages.get(&keys[0]).unwrap().is_some());
        store.messages.usage.insert(sender, saved).unwrap();
        assert_eq!(store.messages.remove_many(&keys).unwrap(), 3);
        store.clear_all().unwrap();
        assert_eq!(store.messages.budget.lock().bytes, 0);
    }

    fn dispatch(id: u8, sender: u8, recipient: u8, payload: usize) -> DispatchP {
        DispatchP {
            to: [recipient; 32].into(),
            from: [sender; 32].into(),
            id: [id; 16].into(),
            payload: vec![0; payload].into(),
            sig: [0; 64].into(),
            accepted_at_ms: now_ms(),
            wake: Wake::No,
            ttl_ms: 0,
        }
    }

    #[test]
    fn byte_limits_cover_both_copies_and_deletion_releases_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let mut row = dispatch(1, 2, 3, 999_000);
        for id in 0..8 {
            row.id = [id; 16].into();
            let queue = if id % 2 == 0 { &store.messages } else { &store.queue };
            assert_eq!(queue.admit(&row, row.accepted_at_ms).unwrap(), QueueAdmission::Insert);
        }
        row.id = [8; 16].into();
        assert_eq!(
            store.queue.admit(&row, row.accepted_at_ms).unwrap(),
            QueueAdmission::Full("sender bytes")
        );
        // The last queued id remains idempotent at capacity, even with a new ingress time.
        row.id = [7; 16].into();
        assert_eq!(
            store.queue.admit(&row, row.accepted_at_ms + 1).unwrap(),
            QueueAdmission::AlreadyQueued
        );
        row.from = [4; 32].into();
        assert_eq!(
            store.queue.admit(&row, row.accepted_at_ms).unwrap(),
            QueueAdmission::IdTakenByOther
        );
        for sender in 4..7 {
            row.from = [sender; 32].into();
            for id in 0..8 {
                row.id = [sender * 10 + id; 16].into();
                assert_eq!(
                    store.queue.admit(&row, row.accepted_at_ms).unwrap(),
                    QueueAdmission::Insert
                );
            }
        }
        row.from = [9; 32].into();
        row.id = [99; 16].into();
        assert_eq!(
            store.queue.admit(&row, row.accepted_at_ms).unwrap(),
            QueueAdmission::Full("recipient bytes")
        );
        let key = store.messages.iter().next().unwrap().key().unwrap();
        store.messages.remove(key.clone()).unwrap();
        store.messages.remove(key).unwrap(); // repeated acknowledgements must not subtract twice
        assert_eq!(store.queue.admit(&row, row.accepted_at_ms).unwrap(), QueueAdmission::Insert);
        let before = store.queue.budget.lock().bytes;
        drop(store);
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.queue.budget.lock().bytes, before);
        assert_eq!(
            reopened.queue.admit(&row, row.accepted_at_ms).unwrap(),
            QueueAdmission::AlreadyQueued
        );
        reopened.clear_all().unwrap();
        assert_eq!(reopened.queue.budget.lock().bytes, 0);
        assert!(reopened.queue.index.len().unwrap() == 0);
    }

    #[test]
    fn concurrent_admissions_share_one_global_budget_including_welcomes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let row = dispatch(1, 2, 3, 100);
        let bytes = (MessageKey::SIZE + row.ser().unwrap().len()) as u64;
        store.messages.admit(&row, row.accepted_at_ms).unwrap();
        store.queue.admit(&row, row.accepted_at_ms).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, 2 * bytes, "both stored copies cost space");
        let key = MessageKey::new(&row.to.0, row.accepted_at_ms, &row.id.0);
        store.messages.remove(key.as_bytes()).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, bytes);
        store.queue.remove(key.as_bytes()).unwrap();
        // Model bytes held by unrelated users, without a six-gigabyte test fixture.
        let baseline = MAX_QUEUE_BYTES - 16 * bytes;
        store.queue.budget.lock().bytes = baseline;
        let accepted = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..32)
                .map(|n| {
                    let mut row = row.clone();
                    row.to = [n; 32].into();
                    let queue = if n % 2 == 0 { &store.messages } else { &store.queue };
                    scope.spawn(move || queue.admit(&row, row.accepted_at_ms).unwrap())
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .filter(|result| *result == QueueAdmission::Insert)
                .count()
        });
        assert_eq!(accepted, 16);
        assert_eq!(store.messages.len().unwrap() + store.queue.len().unwrap(), 16);
        assert_eq!(store.queue.budget.lock().bytes, MAX_QUEUE_BYTES);
        assert_eq!(
            store.welcome.admit_welcome(&[8; 40], &[0; 20]).unwrap(),
            QueueAdmission::Full("relay bytes")
        );
        let queue = if store.messages.len().unwrap() > 0 { &store.messages } else { &store.queue };
        let key = queue.iter().next().unwrap().key().unwrap();
        queue.remove(key).unwrap();
        assert_eq!(
            store.welcome.admit_welcome(&[8; 40], &[0; 20]).unwrap(),
            QueueAdmission::Insert
        );
        assert_eq!(store.queue.budget.lock().bytes, MAX_QUEUE_BYTES - bytes + 60);
        store.welcome.admit_welcome(&[8; 40], &[1; 20]).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, MAX_QUEUE_BYTES - bytes + 60);
        store.welcome.remove([8; 40]).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, MAX_QUEUE_BYTES - bytes);
    }

    #[test]
    fn pressure_reclaims_expired_dispatches_but_preserves_missed_calls() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let mut stale = dispatch(1, 2, 3, 100);
        stale.accepted_at_ms -= 60_000;
        stale.ttl_ms = 1_000;
        let at = stale.accepted_at_ms;
        assert_eq!(store.messages.admit(&stale, at).unwrap(), QueueAdmission::Insert);
        let mut missed = stale.clone();
        missed.id = [2; 16].into();
        missed.wake = Wake::Call;
        assert_eq!(store.queue.admit(&missed, at).unwrap(), QueueAdmission::Insert);
        store.queue.budget.lock().bytes = MAX_QUEUE_BYTES;
        let fresh = dispatch(3, 2, 3, 90);
        assert_eq!(
            store.queue.admit(&fresh, fresh.accepted_at_ms).unwrap(),
            QueueAdmission::Insert
        );
        assert_eq!(store.messages.len().unwrap(), 0);
        assert_eq!(store.queue.len().unwrap(), 2);
    }

    #[test]
    fn expiry_rechecks_a_renewed_welcome_and_restart_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let key = [4; 40];
        store.welcome.admit_welcome(&key, &10u64.to_be_bytes()).unwrap();
        // A sweep observes expiry 10; publication refreshes it before deletion.
        store.welcome.admit_welcome(&key, &30u64.to_be_bytes()).unwrap();
        let expired = |_: &[u8], value: &[u8]| u64::from_be_bytes(value.try_into().unwrap()) <= 20;
        store.welcome.remove_if(key, expired).unwrap();
        assert_eq!(store.welcome.len().unwrap(), 1);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, 48);
        store.welcome.remove_if(key, |_, _| true).unwrap();
        assert_eq!(store.queue.budget.lock().bytes, 0);
    }
}
