//! The relay's fjall store. Durable writes go through [`Store::put_sync`], which leaves the fsync
//! to a maintenance thread that also runs the bounded expiry sweep.

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use common::utils::now_ms;
use fjall::Database;
use fjall::Keyspace;
use fjall::KeyspaceCreateOptions;
use fjall::PersistMode;
use fjall::UserKey;
use fjall::UserValue;

use super::queue::QUEUED_MESSAGE_TTL_MS;
use super::queue::Queue;

pub const KS_MESSAGES: &str = "messages";
pub const KS_DHT_QUEUE: &str = "dht_queue";
pub const KS_DHT_WELCOME: &str = "dht_welcome";
pub const KS_LAST_SEEN: &str = "last_seen";
pub const KS_PRESENCE_CONSENT: &str = "presence_consent";
pub const KS_PRESENCE_STATE: &str = "presence_state";
pub const KS_PRESENCE_LEASE: &str = "presence_lease";
pub const KS_DHT_PUSH_PSEUDONYM: &str = "dht_push_pseudonym";
pub const KS_DHT_PUSH_PENDING: &str = "dht_push_pending";

const PRESENCE_STATE_TTL_MS: u64 = 600_000;

/// How far a presence version may lead its `observed_at_ms`. Honest relays use wall-clock ms and
/// step ahead by one only for updates within the same millisecond.
const PRESENCE_VERSION_MAX_LEAD_MS: u64 = 60_000;

/// Consent grants, last-seen stamps and push pseudonyms are all rewritten when
/// the identity next connects, so this expires quiet identities, not records.
const IDLE_IDENTITY_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// `presence_consent` takes writes for any `(owner, recipient)` pair a DHT peer can sign for,
/// so its size does not follow this relay's own user count.
const MAX_PRESENCE_CONSENT_ROWS: usize = 1_000_000;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const MAX_SWEEP_SCAN: usize = 65_536;
const MAX_SWEEP_REMOVALS: usize = 4_096;

#[cfg(unix)]
const STORE_DIR_MODE: u32 = 0o700;

pub struct Store {
    db: Database,
    pub messages: Queue,
    pub queue: Queue,
    pub key_packages: super::key_packages::KeyPackages,
    pub welcome: Queue,
    /// IPK -> unix ms (u64 BE) when the client last left foreground-active.
    pub last_seen: Keyspace,
    /// `(owner, recipient)` -> newest signed consent or revocation tombstone.
    pub presence_consent: Keyspace,
    pub presence_state: Keyspace,
    pub presence_lease: Keyspace,
    pub push_pseudonym: Keyspace,
    pub push_pending: Keyspace,
    /// Striped by the row's owner, so a cap or version check sees every earlier write.
    admission: [parking_lot::Mutex<()>; 64],
    maintenance: Arc<Maintenance>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        std::fs::create_dir_all(path).context("create store directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(STORE_DIR_MODE))
                .context("restrict store directory permissions")?;
        }

        let db = Database::builder(path).open().context("open fjall database")?;
        let open = |name: &str| {
            db.keyspace(name, KeyspaceCreateOptions::default)
                .with_context(|| format!("open `{name}`"))
        };
        let messages = open(KS_MESSAGES)?;
        let queue = open(KS_DHT_QUEUE)?;
        let key_packages = super::key_packages::KeyPackages::open(&db)?;
        let welcome = open(KS_DHT_WELCOME)?;
        let [messages, queue, welcome] =
            Queue::open(&db, messages, queue, welcome).context("index queued messages")?;
        let last_seen = open(KS_LAST_SEEN)?;
        let presence_consent = open(KS_PRESENCE_CONSENT)?;
        let presence_state = open(KS_PRESENCE_STATE)?;
        let presence_lease = open(KS_PRESENCE_LEASE)?;
        let push_pseudonym = open(KS_DHT_PUSH_PSEUDONYM)?;
        let push_pending = open(KS_DHT_PUSH_PENDING)?;

        let maintenance = Arc::new(Maintenance::default());
        let targets = vec![
            SweepTarget::new(&key_packages.spent, super::key_packages::spent_expired),
            SweepTarget::queue(&messages, queued_message_expired),
            SweepTarget::queue(&queue, queued_message_expired),
            SweepTarget::queue(&welcome, welcome_expired),
            SweepTarget::new(&last_seen, last_seen_expired),
            SweepTarget::new(&presence_consent, presence_consent_expired),
            SweepTarget::new(&presence_state, presence_state_expired),
            SweepTarget::new(&presence_lease, presence_lease_expired),
            SweepTarget::new(&push_pseudonym, push_pseudonym_expired),
        ];
        let worker = std::thread::Builder::new()
            .name("pz-store-maint".into())
            .spawn({
                let db = db.clone();
                let maintenance = maintenance.clone();
                move || run_maintenance(db, targets, maintenance)
            })
            .context("spawn store maintenance thread")?;

        Ok(Self {
            db,
            messages,
            queue,
            key_packages,
            welcome,
            last_seen,
            presence_consent,
            presence_state,
            presence_lease,
            push_pseudonym,
            push_pending,
            admission: std::array::from_fn(|_| parking_lot::Mutex::new(())),
            maintenance,
            worker: Some(worker),
        })
    }

    /// Held from a cap or version check through the write it admits.
    pub fn admission(&self, owner: &[u8; 32]) -> parking_lot::MutexGuard<'_, ()> {
        self.admission[usize::from(owner[0]) % self.admission.len()].lock()
    }

    /// Not fsynced: a stamp lost in a crash only degrades last-seen.
    pub fn put_last_seen(&self, ipk: &[u8; 32], ts_ms: u64) -> fjall::Result<()> {
        self.last_seen.insert(ipk, ts_ms.to_be_bytes())
    }

    pub fn get_last_seen(&self, ipk: &[u8; 32]) -> Option<u64> {
        let v = self.last_seen.get(ipk).ok().flatten()?;
        Some(u64::from_be_bytes(v.as_ref().try_into().ok()?))
    }

    /// Value layout: `version (u64 BE) || issued_at_ms (u64 BE) || granted (u8)`.
    pub fn put_presence_consent(
        &self, consent: &common::proto::dht_p2p::PresenceConsent,
    ) -> fjall::Result<bool> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(&consent.owner.0);
        key[32..].copy_from_slice(&consent.recipient.0);
        let _admission = self.admission(&consent.owner.0);
        let stored = self.presence_consent.get(key)?;
        if stored.as_ref().is_some_and(|v| be_u64(v, 0).is_some_and(|old| old >= consent.version)) {
            return Ok(false);
        }
        if stored.is_none() && self.presence_consent.approximate_len() >= MAX_PRESENCE_CONSENT_ROWS
        {
            return Ok(false);
        }
        let mut value = Vec::with_capacity(17);
        value.extend_from_slice(&consent.version.to_be_bytes());
        value.extend_from_slice(&consent.issued_at_ms.to_be_bytes());
        value.push(consent.granted as u8);
        self.put_sync(&self.presence_consent, key, value)?;
        Ok(true)
    }

    pub fn has_presence_consent(&self, ipk: &[u8; 32], contact: &[u8; 32]) -> bool {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(ipk);
        key[32..].copy_from_slice(contact);
        self.presence_consent.get(key).ok().flatten().is_some_and(|v| v.get(16) == Some(&1))
    }

    /// Value: `version || observed_at_ms || tag (u8) || timestamp || expires_at_ms`, u64s BE.
    /// `RelayPresenceState::verify` skew-checks `observed_at_ms`, so it is the staleness clock.
    pub fn put_presence_state(
        &self, recipient: &[u8; 32], contact: &[u8; 32],
        state: &common::proto::client_rel::PresenceState, version: u64, observed_at_ms: u64,
        lease_expires_at_ms: u64,
    ) -> fjall::Result<bool> {
        if version > observed_at_ms.saturating_add(PRESENCE_VERSION_MAX_LEAD_MS) {
            return Ok(false);
        }
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(recipient);
        key[32..].copy_from_slice(contact);
        let _admission = self.admission(recipient);
        if self.presence_state.get(key)?.is_some_and(|v| {
            !presence_state_expired(b"", &v, observed_at_ms)
                && (be_u64(&v, 0).is_some_and(|old| old >= version)
                    || be_u64(&v, 8).is_some_and(|old| old >= observed_at_ms))
        }) {
            return Ok(false);
        }
        let (tag, timestamp) = match state {
            common::proto::client_rel::PresenceState::Online => (0, 0),
            common::proto::client_rel::PresenceState::Idle { since } => (1, *since),
            common::proto::client_rel::PresenceState::Offline { last_seen } => (2, *last_seen),
        };
        // Clamped so a relay cannot pin a user online with a distant lease.
        let expires_at_ms =
            lease_expires_at_ms.min(observed_at_ms.saturating_add(PRESENCE_STATE_TTL_MS));
        let mut value = Vec::with_capacity(33);
        value.extend_from_slice(&version.to_be_bytes());
        value.extend_from_slice(&observed_at_ms.to_be_bytes());
        value.push(tag);
        value.extend_from_slice(&timestamp.to_be_bytes());
        value.extend_from_slice(&expires_at_ms.to_be_bytes());
        self.presence_state.insert(key, value)?;
        Ok(true)
    }
    pub fn get_presence_state(
        &self, recipient: &[u8; 32], contact: &[u8; 32],
    ) -> Option<common::proto::client_rel::PresenceState> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(recipient);
        key[32..].copy_from_slice(contact);
        let value = self.presence_state.get(key).ok().flatten()?;
        if presence_state_expired(b"", &value, now_ms()) {
            return None;
        }
        let value = value.as_ref();
        let timestamp = u64::from_be_bytes(value.get(17..25)?.try_into().ok()?);
        match *value.get(16)? {
            0 => Some(common::proto::client_rel::PresenceState::Online),
            1 => Some(common::proto::client_rel::PresenceState::Idle { since: timestamp }),
            2 => Some(common::proto::client_rel::PresenceState::Offline { last_seen: timestamp }),
            _ => None,
        }
    }

    pub fn put_presence_lease(
        &self, lease: &common::proto::dht_p2p::PresenceLease,
    ) -> fjall::Result<bool> {
        use common::proto::pack::Packer;
        use common::proto::pack::Unpacker;

        let _admission = self.admission(&lease.user.0);
        if self.presence_lease.get(&lease.user.0)?.is_some_and(|v| {
            common::proto::dht_p2p::PresenceLease::deser(&v)
                .ok()
                .is_some_and(|old| old.version >= lease.version)
        }) {
            return Ok(false);
        }
        let Ok(value) = lease.ser() else { return Ok(false) };
        self.put_sync(&self.presence_lease, &lease.user.0, value)?;
        Ok(true)
    }

    pub fn get_presence_lease(
        &self, user: &[u8; 32],
    ) -> Option<common::proto::dht_p2p::PresenceLease> {
        use common::proto::pack::Unpacker;

        common::proto::dht_p2p::PresenceLease::deser(&self.presence_lease.get(user).ok().flatten()?)
            .ok()
    }

    /// Value: `pseudonym (32) || refreshed_at_ms (u64 BE)`. The pseudonym reveals no platform
    /// token without the push gateway's database.
    pub fn put_push_pseudonym(&self, ipk: &[u8; 32], pseudonym: &[u8; 32]) -> fjall::Result<()> {
        let mut value = Vec::with_capacity(40);
        value.extend_from_slice(pseudonym);
        value.extend_from_slice(&now_ms().to_be_bytes());
        self.put_sync(&self.push_pseudonym, ipk, value)
    }

    pub fn get_push_pseudonym(&self, ipk: &[u8; 32]) -> Option<[u8; 32]> {
        let value = self.push_pseudonym.get(ipk).ok().flatten()?;
        value.get(..32)?.try_into().ok()
    }

    pub fn put_pending_push(
        &self, publish: &common::proto::dht_p2p::PushPseudonymPublish,
    ) -> fjall::Result<()> {
        use common::proto::pack::Packer;

        let Ok(value) = publish.ser() else { return Ok(()) };
        self.put_sync(&self.push_pending, &publish.user_ipk.0, value)
    }

    pub fn remove_pending_push(&self, ipk: &[u8; 32]) -> fjall::Result<()> {
        self.push_pending.remove(ipk)?;
        self.request_persist();
        Ok(())
    }

    pub fn pending_pushes(&self) -> Vec<common::proto::dht_p2p::PushPseudonymPublish> {
        use common::proto::pack::Unpacker;

        self.push_pending
            .iter()
            .filter_map(|entry| {
                entry.into_inner().ok().and_then(|(_, value)| {
                    common::proto::dht_p2p::PushPseudonymPublish::deser(&value).ok()
                })
            })
            .collect()
    }

    /// The fsync is left to the maintenance thread's group commit. A failed fsync poisons the
    /// fjall database, so the next write on any keyspace surfaces it as `Error::Poisoned`.
    pub fn put_sync(
        &self, ks: &Keyspace, key: impl Into<UserKey>, val: impl Into<UserValue>,
    ) -> fjall::Result<()> {
        ks.insert(key, val)?;
        self.request_persist();
        Ok(())
    }

    fn request_persist(&self) -> u64 {
        let mut state = self.maintenance.lock();
        state.persist_requested = true;
        state.requested_gen += 1;
        let requested = state.requested_gen;
        drop(state);
        self.maintenance.wake.notify_one();
        requested
    }

    /// Covers every write issued so far. Awaiting it is the durability point: never acknowledge a
    /// write to a peer before it resolves.
    pub fn persist_barrier(&self) -> PersistBarrier {
        PersistBarrier { maintenance: self.maintenance.clone(), target: self.request_persist() }
    }

    /// Clears every keyspace on disk; in-memory routing and connections stay.
    pub fn clear_all(&self) -> Result<usize> {
        let mut n = self.messages.len()? + self.queue.len()? + self.welcome.len()?;
        self.messages.clear()?;
        self.queue.clear()?;
        self.welcome.clear()?;
        for ks in [
            &self.key_packages.records,
            &self.key_packages.spent,
            &self.last_seen,
            &self.presence_consent,
            &self.presence_state,
            &self.presence_lease,
            &self.push_pseudonym,
            &self.push_pending,
        ] {
            n += ks.len().context("count keyspace")?;
            ks.clear().context("clear keyspace")?;
        }
        self.db.persist(PersistMode::SyncAll).context("persist after clear")?;
        Ok(n)
    }
}

#[cfg(test)]
impl Store {
    /// A new empty store at `path`. Creating fjall keyspaces syncs each one to disk, most of a
    /// second on macOS, so they are created once per test process and their files copied.
    pub(crate) fn open_empty(path: &Path) -> Self {
        type Entry = (std::path::PathBuf, Option<Vec<u8>>);
        static LAYOUT: std::sync::LazyLock<Vec<Entry>> = std::sync::LazyLock::new(|| {
            let dir = tempfile::tempdir().unwrap();
            drop(Store::open(dir.path()).unwrap());
            let mut entries = Vec::new();
            let mut folders = vec![dir.path().to_path_buf()];
            while let Some(folder) = folders.pop() {
                for entry in std::fs::read_dir(folder).unwrap() {
                    let path = entry.unwrap().path();
                    let relative = path.strip_prefix(dir.path()).unwrap().to_path_buf();
                    if path.is_dir() {
                        entries.push((relative, None));
                        folders.push(path);
                    } else {
                        entries.push((relative, Some(std::fs::read(&path).unwrap())));
                    }
                }
            }
            entries
        });
        for (relative, contents) in LAYOUT.iter() {
            let target = path.join(relative);
            match contents {
                None => std::fs::create_dir_all(target).unwrap(),
                Some(bytes) => {
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, bytes).unwrap();
                },
            }
        }
        Self::open(path).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.maintenance.lock().shutdown = true;
        self.maintenance.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct Maintenance {
    state: Mutex<MaintenanceState>,
    wake: Condvar,
    done: Condvar,
}

#[derive(Default)]
struct MaintenanceState {
    persist_requested: bool,
    shutdown: bool,
    /// Bumped per write; the commit that observes a value covers every write
    /// numbered at or below it.
    requested_gen: u64,
    persisted_gen: u64,
    persist_failed: bool,
}

impl Maintenance {
    fn lock(&self) -> MutexGuard<'_, MaintenanceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct PersistBarrier {
    maintenance: Arc<Maintenance>,
    target: u64,
}

impl PersistBarrier {
    pub async fn wait(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.wait_blocking())
            .await
            .context("persist barrier task")?
    }

    fn wait_blocking(&self) -> Result<()> {
        let mut state = self.maintenance.lock();
        while state.persisted_gen < self.target && !state.shutdown {
            state = self.maintenance.done.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        if state.persist_failed {
            bail!("relay store fsync failed; the database is poisoned");
        }
        Ok(())
    }
}

type ExpiryFn = fn(&[u8], &[u8], u64) -> bool;

struct SweepTarget {
    ks: Keyspace,
    expired: ExpiryFn,
    cursor: Option<UserKey>,
    queue: Option<Queue>,
}

impl SweepTarget {
    fn queue(queue: &Queue, expired: ExpiryFn) -> Self {
        Self { ks: queue.ks.clone(), expired, cursor: None, queue: Some(queue.clone()) }
    }

    fn new(ks: &Keyspace, expired: ExpiryFn) -> Self {
        Self { ks: ks.clone(), expired, cursor: None, queue: None }
    }
}

/// Group-commit fsync and the expiry sweep, on a dedicated thread off the tokio workers.
fn run_maintenance(db: Database, mut targets: Vec<SweepTarget>, maintenance: Arc<Maintenance>) {
    let mut next_sweep = Instant::now() + SWEEP_INTERVAL;
    loop {
        let mut state = maintenance.lock();
        while !state.persist_requested && !state.shutdown {
            let wait = next_sweep.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                break;
            }
            state = maintenance
                .wake
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let shutdown = state.shutdown;
        let persist = std::mem::take(&mut state.persist_requested);
        // Snapshotted under the lock: this commit covers exactly the writes
        // numbered at or below it.
        let covered = state.requested_gen;
        drop(state);

        if persist || shutdown {
            let result = db.persist(PersistMode::SyncAll);
            let mut state = maintenance.lock();
            if let Err(e) = &result {
                common::error!("relay store fsync failed: {e}");
                state.persist_failed = true;
            }
            state.persisted_gen = covered;
            drop(state);
            maintenance.done.notify_all();
        }
        if shutdown {
            maintenance.done.notify_all();
            return;
        }
        if Instant::now() >= next_sweep {
            let now = now_ms();
            for target in &mut targets {
                sweep(target, now);
            }
            next_sweep = Instant::now() + SWEEP_INTERVAL;
        }
    }
}

/// One bounded pass over `target`, resuming where the previous pass ran out of
/// budget so a keyspace larger than [`MAX_SWEEP_SCAN`] still drains fully.
fn sweep(target: &mut SweepTarget, now_ms: u64) {
    let start = match target.cursor.take() {
        Some(key) => Bound::Excluded(key),
        None => Bound::Unbounded,
    };
    let mut scanned = 0usize;
    let mut expired: Vec<UserKey> = Vec::new();
    let mut resume = None;

    for guard in target.ks.range::<UserKey, _>((start, Bound::Unbounded)) {
        let Ok((key, value)) = guard.into_inner() else { break };
        if (target.expired)(&key, &value, now_ms) {
            expired.push(key.clone());
        }
        scanned += 1;
        if scanned >= MAX_SWEEP_SCAN || expired.len() >= MAX_SWEEP_REMOVALS {
            resume = Some(key);
            break;
        }
    }

    if let Some(queue) = &target.queue {
        let _ = queue.remove_many_if(&expired, |key, value| (target.expired)(key, value, now_ms));
    } else {
        for key in expired {
            let _ = target.ks.remove(key);
        }
    }
    target.cursor = resume;
}

/// Retention uses the stored key's clock; shorter sender TTLs also release space, while
/// call offers remain available to record missed calls until normal retention expires.
fn queued_message_expired(key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(key, 32)
        .is_none_or(|accepted_at| now_ms.saturating_sub(accepted_at) > QUEUED_MESSAGE_TTL_MS)
        || super::MessageKey::parse(key)
            .and_then(|key| super::queued_dispatch(&key.recipient, value))
            .is_some_and(|dispatch| dispatch.is_expired(now_ms))
}

fn welcome_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 0).is_none_or(|expires| expires <= now_ms)
}

fn last_seen_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 0).is_none_or(|ts| now_ms.saturating_sub(ts) > IDLE_IDENTITY_TTL_MS)
}

fn presence_consent_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 8).is_none_or(|issued_at| now_ms.saturating_sub(issued_at) > IDLE_IDENTITY_TTL_MS)
}

/// Presence dies at its stored deadline. Older 25-byte rows carry no deadline and fall back to
/// the fixed ceiling.
fn presence_state_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 25)
        .or_else(|| be_u64(value, 8).map(|at| at.saturating_add(PRESENCE_STATE_TTL_MS)))
        .is_none_or(|deadline| now_ms >= deadline)
}

fn presence_lease_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    use common::proto::pack::Unpacker;

    common::proto::dht_p2p::PresenceLease::deser(value)
        .ok()
        .is_none_or(|lease| now_ms > lease.expires_at_ms)
}

/// Rows in the older 32-byte shape carry no stamp and are kept.
fn push_pseudonym_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 32)
        .is_some_and(|refreshed_at| now_ms.saturating_sub(refreshed_at) > IDLE_IDENTITY_TTL_MS)
}

fn be_u64(value: &[u8], offset: usize) -> Option<u64> {
    value.get(offset..offset + 8).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes)
}

#[cfg(test)]
mod tests {
    use common::proto::client_rel::PresenceState;
    use common::proto::dht_p2p::PresenceConsent;

    use super::*;

    /// The barrier is the custody point every `Stored` and `Queued` waits on: it resolves after
    /// the commit covering the writes before it, and a store shutting down still releases it.
    #[tokio::test]
    async fn barriers_resolve_after_their_commit_even_when_the_store_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        for n in 0..8u8 {
            store.messages.insert([n], [n]).unwrap();
            store.request_persist();
        }
        let barrier = store.persist_barrier();
        let covers = barrier.target;
        tokio::time::timeout(Duration::from_secs(5), barrier.wait()).await.unwrap().unwrap();
        assert!(store.maintenance.lock().persisted_gen >= covers);

        store.messages.insert([8], [8]).unwrap();
        store.request_persist();
        let pending = store.persist_barrier();
        drop(store);
        tokio::time::timeout(Duration::from_secs(5), pending.wait()).await.unwrap().unwrap();
        assert_eq!(Store::open(dir.path()).unwrap().messages.len().unwrap(), 9);
    }

    /// Expiry removes only expired rows, and a keyspace larger than one pass's budget drains
    /// over passes that resume where the last one stopped.
    #[test]
    fn the_sweep_removes_only_expired_rows_and_resumes_from_its_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let now = 10 * IDLE_IDENTITY_TTL_MS;
        store.put_last_seen(&[0xFF; 32], now).unwrap();
        for n in 0..MAX_SWEEP_REMOVALS as u64 + 32 {
            let mut ipk = [0; 32];
            ipk[..8].copy_from_slice(&n.to_be_bytes());
            store.put_last_seen(&ipk, now - IDLE_IDENTITY_TTL_MS - 1).unwrap();
        }
        let mut target = SweepTarget::new(&store.last_seen, last_seen_expired);
        sweep(&mut target, now);
        assert!(target.cursor.is_some(), "the budget ran out mid-keyspace");
        assert_eq!(store.last_seen.len().unwrap(), 33);
        sweep(&mut target, now);
        assert!(target.cursor.is_none());
        assert_eq!(store.last_seen.len().unwrap(), 1);
        assert_eq!(store.get_last_seen(&[0xFF; 32]), Some(now));
    }

    /// A replayed consent cannot undo a revocation, a version far ahead of its observation
    /// cannot pin a presence row, and a stale row counts as absent.
    #[test]
    fn presence_rows_refuse_replays_and_versions_that_run_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let consent = |version, granted| PresenceConsent {
            owner: [1; 32].into(),
            recipient: [2; 32].into(),
            version,
            issued_at_ms: 1,
            granted,
            user_sig: [0; 64].into(),
        };
        for (label, sent, stored, granted) in [
            ("grant", consent(7, true), true, true),
            ("older replay", consent(6, true), false, true),
            ("same version", consent(7, true), false, true),
            ("revocation", consent(8, false), true, false),
            ("replayed grant", consent(8, true), false, false),
        ] {
            assert_eq!(store.put_presence_consent(&sent).unwrap(), stored, "{label}");
            assert_eq!(store.has_presence_consent(&[1; 32], &[2; 32]), granted, "{label}");
        }

        let put = |version, observed_at: u64| {
            let deadline = observed_at + PRESENCE_STATE_TTL_MS;
            store
                .put_presence_state(
                    &[1; 32],
                    &[2; 32],
                    &PresenceState::Online,
                    version,
                    observed_at,
                    deadline,
                )
                .unwrap()
        };
        let now = now_ms();
        assert!(!put(u64::MAX, now), "a far-future version is refused");
        assert_eq!(store.get_presence_state(&[1; 32], &[2; 32]), None);
        let t0 = 1_700_000_000_000;
        assert!(put(t0 + PRESENCE_VERSION_MAX_LEAD_MS, t0));
        assert!(
            !put(t0 + PRESENCE_VERSION_MAX_LEAD_MS, t0 + 1),
            "a live row still wins on version"
        );
        assert!(put(1, t0 + PRESENCE_STATE_TTL_MS + 1), "a stale row counts as absent");
    }

    /// Racing writes of one consent row keep the newest version, so an older grant written late
    /// cannot undo a revocation.
    #[test]
    fn racing_presence_writes_keep_the_newest_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        for round in 0..20u8 {
            std::thread::scope(|scope| {
                for version in 1..=8 {
                    let consent = PresenceConsent {
                        owner: [round; 32].into(),
                        recipient: [2; 32].into(),
                        version,
                        issued_at_ms: 1,
                        granted: version < 8,
                        user_sig: [0; 64].into(),
                    };
                    let store = &store;
                    scope.spawn(move || store.put_presence_consent(&consent).unwrap());
                }
            });
            let row = store.presence_consent.get([[round; 32], [2; 32]].concat()).unwrap();
            assert_eq!(row.and_then(|v| be_u64(&v, 0)), Some(8), "round {round}");
            assert!(!store.has_presence_consent(&[round; 32], &[2; 32]), "round {round}");
        }
    }
}
