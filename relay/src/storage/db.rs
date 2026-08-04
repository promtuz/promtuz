//! The relay's on-disk store: one fjall `Database`, several keyspaces.
//!
//! Keyspaces (fjall's column-family equivalent — each its own LSM-tree):
//! - `messages`       sender-relay local fallback queue (`MessageKey` -> DeliverP).
//! - `dht_queue`      home-replica offline queue (`MessageKey`, per-recipient prefix).
//! - `dht_keypackage` MLS KeyPackage stash (per-IPK prefix).
//! - `dht_welcome`    MLS Welcome stash (per-recipient prefix).
//!
//! fjall does exact prefix scans natively, so no prefix-extractor config is
//! needed (unlike RocksDB). Durability-critical writes go through
//! [`Store::put_sync`], which hands the journal fsync to the store's
//! maintenance thread; everything else is journal-buffered. That thread also
//! runs the bounded expiry sweep over the presence/identity keyspaces.

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
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use fjall::Database;
use fjall::Keyspace;
use fjall::KeyspaceCreateOptions;
use fjall::PersistMode;
use fjall::UserKey;
use fjall::UserValue;

pub const KS_MESSAGES: &str = "messages";
pub const KS_DHT_QUEUE: &str = "dht_queue";
pub const KS_DHT_KEYPACKAGE: &str = "dht_keypackage";
pub const KS_DHT_WELCOME: &str = "dht_welcome";
pub const KS_LAST_SEEN: &str = "last_seen";
pub const KS_PRESENCE_CONSENT: &str = "presence_consent";
pub const KS_PRESENCE_STATE: &str = "presence_state";
pub const KS_PRESENCE_LEASE: &str = "presence_lease";
pub const KS_DHT_PUSH_PSEUDONYM: &str = "dht_push_pseudonym";
pub const KS_DHT_PUSH_PENDING: &str = "dht_push_pending";

/// Mirrors `dht::config::PRESENCE_TTL_MS`; duplicated because the `ldb` lib
/// target compiles `storage` without the DHT module.
const PRESENCE_STATE_TTL_MS: u64 = 600_000;

/// How far a presence version may lead its own `observed_at_ms`. Honest relays
/// derive the version from wall-clock milliseconds and only step ahead of it by
/// one per update within the same millisecond.
const PRESENCE_VERSION_MAX_LEAD_MS: u64 = 60_000;

/// Consent grants, last-seen stamps and push pseudonyms are all rewritten when
/// the identity next connects, so this expires quiet identities, not records.
const IDLE_IDENTITY_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// How long an undelivered message is held before the sweep drops it. Matches
/// the Welcome retention window, so a recipient offline past it loses both.
const QUEUED_MESSAGE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Ceiling on `presence_consent` rows. The keyspace takes writes for any
/// `(owner, recipient)` pair a DHT peer can sign for, so its size is not a
/// function of this relay's own user count.
const MAX_PRESENCE_CONSENT_ROWS: usize = 1_000_000;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const MAX_SWEEP_SCAN: usize = 65_536;
const MAX_SWEEP_REMOVALS: usize = 4_096;

#[cfg(unix)]
const STORE_DIR_MODE: u32 = 0o700;

/// Owns the relay's fjall `Database` and its keyspace handles. Shared as
/// `Arc<Store>` between the `Relay` (message queue) and the `Dht` (home
/// queue, MLS stashes) — both point at the same on-disk store.
pub struct Store {
    db:                   Database,
    pub messages:         Keyspace,
    pub queue:            Keyspace,
    pub keypackage:       Keyspace,
    pub welcome:          Keyspace,
    /// IPK (32B) -> last-disconnect unix-ms (u64 BE). Powers presence last-seen.
    pub last_seen:        Keyspace,
    /// `(owner, recipient)` -> newest signed consent or revocation tombstone.
    pub presence_consent: Keyspace,
    pub presence_state:   Keyspace,
    pub presence_lease:   Keyspace,
    pub push_pseudonym:   Keyspace,
    pub push_pending:     Keyspace,
    maintenance:          Arc<Maintenance>,
    worker:               Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Store {
    // fjall's `Database` / `Keyspace` handles aren't `Debug`; `Dht` and
    // `Relay` derive `Debug` and hold an `Arc<Store>`, so give them a stub.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating if absent) the relay's fjall store at `path`.
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
        let messages =
            db.keyspace(KS_MESSAGES, KeyspaceCreateOptions::default).context("open `messages`")?;
        let queue = db
            .keyspace(KS_DHT_QUEUE, KeyspaceCreateOptions::default)
            .context("open `dht_queue`")?;
        let keypackage = db
            .keyspace(KS_DHT_KEYPACKAGE, KeyspaceCreateOptions::default)
            .context("open `dht_keypackage`")?;
        let welcome = db
            .keyspace(KS_DHT_WELCOME, KeyspaceCreateOptions::default)
            .context("open `dht_welcome`")?;
        let last_seen = db
            .keyspace(KS_LAST_SEEN, KeyspaceCreateOptions::default)
            .context("open `last_seen`")?;
        let presence_consent = db
            .keyspace(KS_PRESENCE_CONSENT, KeyspaceCreateOptions::default)
            .context("open `presence_consent`")?;
        let presence_state = db
            .keyspace(KS_PRESENCE_STATE, KeyspaceCreateOptions::default)
            .context("open `presence_state`")?;
        let presence_lease = db
            .keyspace(KS_PRESENCE_LEASE, KeyspaceCreateOptions::default)
            .context("open `presence_lease`")?;
        let push_pseudonym = db
            .keyspace(KS_DHT_PUSH_PSEUDONYM, KeyspaceCreateOptions::default)
            .context("open `dht_push_pseudonym`")?;
        let push_pending = db
            .keyspace(KS_DHT_PUSH_PENDING, KeyspaceCreateOptions::default)
            .context("open `dht_push_pending`")?;

        let maintenance = Arc::new(Maintenance::default());
        let targets = vec![
            SweepTarget::new(&messages, queued_message_expired),
            SweepTarget::new(&queue, queued_message_expired),
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
            keypackage,
            welcome,
            last_seen,
            presence_consent,
            presence_state,
            presence_lease,
            push_pseudonym,
            push_pending,
            maintenance,
            worker: Some(worker),
        })
    }

    /// Record when a peer was last foreground-active (unix-ms) — stamped only on
    /// leaving an Active state, never on a background connect/disconnect, so a
    /// wake doesn't read as "seen now". Buffered, not fsynced — a lost stamp on
    /// crash just degrades to "last-seen unknown".
    pub fn put_last_seen(&self, ipk: &[u8; 32], ts_ms: u64) -> fjall::Result<()> {
        self.last_seen.insert(ipk, ts_ms.to_be_bytes())
    }

    /// Read a peer's last-disconnect time, `None` if never recorded.
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

    /// Value layout:
    /// `version (u64 BE) || observed_at_ms (u64 BE) || tag (u8) || timestamp (u64 BE)`.
    ///
    /// `observed_at_ms` is verified within `PRESENCE_STATE_MAX_SKEW_MS` of real
    /// time by `RelayPresenceState::verify`, so it doubles as the clock for the
    /// staleness comparison against the stored row.
    pub fn put_presence_state(
        &self, recipient: &[u8; 32], contact: &[u8; 32],
        state: &common::proto::client_rel::PresenceState, version: u64, observed_at_ms: u64,
    ) -> fjall::Result<bool> {
        if version > observed_at_ms.saturating_add(PRESENCE_VERSION_MAX_LEAD_MS) {
            return Ok(false);
        }
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(recipient);
        key[32..].copy_from_slice(contact);
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
        let mut value = Vec::with_capacity(25);
        value.extend_from_slice(&version.to_be_bytes());
        value.extend_from_slice(&observed_at_ms.to_be_bytes());
        value.push(tag);
        value.extend_from_slice(&timestamp.to_be_bytes());
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

    /// Durable home-side `IPK -> P` mapping. `P` is opaque to the relay and
    /// cannot reveal a platform token without the push gateway's database.
    /// Value layout: `pseudonym (32B) || refreshed_at_ms (u64 BE)`.
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

    /// Insert, then hand the journal fsync to the maintenance thread, which
    /// coalesces concurrent requests into one `SyncAll`. The value is in the
    /// journal buffer on return; the group commit closes the machine-crash
    /// window. A failed fsync poisons the fjall database, so the next write on
    /// any keyspace surfaces it as `Error::Poisoned`.
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

    /// Take a barrier covering every write issued so far. Awaiting it is the
    /// durability point: a caller that acknowledges a write to a peer must not
    /// reply before the barrier resolves.
    pub fn persist_barrier(&self) -> PersistBarrier {
        PersistBarrier { maintenance: self.maintenance.clone(), target: self.request_persist() }
    }

    /// A buffered, atomic multi-op batch (used for drain GC). Not fsynced — a
    /// crash re-delivers, and the client dedupes by id.
    pub fn batch(&self) -> fjall::OwnedWriteBatch {
        self.db.batch()
    }

    /// Truncate every keyspace and fsync, returning the number of entries that
    /// were live. Live-safe: the relay owns the fjall writer, so no lock fight
    /// — the `pzrelay clear-db` reset path. Leaves the daemon's in-memory
    /// routing/connections intact.
    pub fn clear_all(&self) -> Result<usize> {
        let mut n = 0usize;
        for ks in [
            &self.messages,
            &self.queue,
            &self.keypackage,
            &self.welcome,
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
    wake:  Condvar,
    /// Signalled after each group commit, for [`PersistBarrier`] waiters.
    done:  Condvar,
}

#[derive(Default)]
struct MaintenanceState {
    persist_requested: bool,
    shutdown:          bool,
    /// Bumped per write; the commit that observes a value covers every write
    /// numbered at or below it.
    requested_gen:     u64,
    persisted_gen:     u64,
    persist_failed:    bool,
}

impl Maintenance {
    fn lock(&self) -> MutexGuard<'_, MaintenanceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Resolves once the group commit covering the writes behind it has hit disk.
pub struct PersistBarrier {
    maintenance: Arc<Maintenance>,
    target:      u64,
}

impl PersistBarrier {
    /// Waits on the blocking pool, so a tokio worker is never parked on fsync.
    pub async fn wait(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.wait_blocking())
            .await
            .context("persist barrier task")?
    }

    fn wait_blocking(&self) -> Result<()> {
        let mut state = self.maintenance.lock();
        while state.persisted_gen < self.target && !state.shutdown {
            state = self
                .maintenance
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.persist_failed {
            bail!("relay store fsync failed; the database is poisoned");
        }
        Ok(())
    }
}

type ExpiryFn = fn(&[u8], &[u8], u64) -> bool;

struct SweepTarget {
    ks:      Keyspace,
    expired: ExpiryFn,
    cursor:  Option<UserKey>,
}

impl SweepTarget {
    fn new(ks: &Keyspace, expired: ExpiryFn) -> Self {
        Self { ks: ks.clone(), expired, cursor: None }
    }
}

/// Group-commit fsync plus the periodic expiry sweep, on a thread of its own so
/// neither ever runs on a tokio worker.
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

    for key in expired {
        let _ = target.ks.remove(key);
    }
    target.cursor = resume;
}

/// Queue rows carry their acceptance time in the key
/// (`recipient(32) || ts_be(8) || id(16)`), so this needs no value decode.
fn queued_message_expired(key: &[u8], _value: &[u8], now_ms: u64) -> bool {
    be_u64(key, 32).is_none_or(|accepted_at| {
        now_ms.saturating_sub(accepted_at) > QUEUED_MESSAGE_TTL_MS
    })
}

fn last_seen_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 0).is_none_or(|ts| now_ms.saturating_sub(ts) > IDLE_IDENTITY_TTL_MS)
}

fn presence_consent_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 8).is_none_or(|issued_at| now_ms.saturating_sub(issued_at) > IDLE_IDENTITY_TTL_MS)
}

fn presence_state_expired(_key: &[u8], value: &[u8], now_ms: u64) -> bool {
    be_u64(value, 8)
        .is_none_or(|observed_at| now_ms.saturating_sub(observed_at) > PRESENCE_STATE_TTL_MS)
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

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use common::proto::client_rel::PresenceState;
    use common::proto::dht_p2p::PresenceConsent;

    use super::*;

    fn fresh_store() -> Store {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("pz-cleardb-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Store::open(&path).expect("open store")
    }

    fn consent(version: u64, issued_at_ms: u64, granted: bool) -> PresenceConsent {
        PresenceConsent {
            owner: [1u8; 32].into(),
            recipient: [2u8; 32].into(),
            version,
            issued_at_ms,
            granted,
            user_sig: [0u8; 64].into(),
        }
    }

    #[test]
    fn clear_all_empties_every_keyspace() {
        let store = fresh_store();
        store.messages.insert("a".as_bytes(), "1".as_bytes()).unwrap();
        store.queue.insert("b".as_bytes(), "2".as_bytes()).unwrap();
        store.keypackage.insert("c".as_bytes(), "3".as_bytes()).unwrap();
        store.welcome.insert("d".as_bytes(), "4".as_bytes()).unwrap();
        store.last_seen.insert("e".as_bytes(), "5".as_bytes()).unwrap();

        let n = store.clear_all().expect("clear");
        assert_eq!(n, 5, "must report every deleted entry");
        for ks in
            [&store.messages, &store.queue, &store.keypackage, &store.welcome, &store.last_seen]
        {
            assert_eq!(ks.iter().count(), 0, "keyspace must be empty after clear");
        }
    }

    #[cfg(unix)]
    #[test]
    fn store_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("pz-mode-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _store = Store::open(&path).expect("open store");

        let mode = std::fs::metadata(&path).expect("stat store").permissions().mode();
        assert_eq!(mode & 0o777, STORE_DIR_MODE);
    }

    #[test]
    fn last_seen_roundtrips_and_defaults_to_none() {
        let store = fresh_store();
        let ipk = [7u8; 32];
        assert_eq!(store.get_last_seen(&ipk), None, "unrecorded IPK is None");
        store.put_last_seen(&ipk, 1_700_000_000_000).unwrap();
        assert_eq!(store.get_last_seen(&ipk), Some(1_700_000_000_000));
    }

    #[test]
    fn push_pseudonym_roundtrips() {
        let store = fresh_store();
        let ipk = [3u8; 32];
        assert_eq!(store.get_push_pseudonym(&ipk), None);
        store.put_push_pseudonym(&ipk, &[9u8; 32]).unwrap();
        assert_eq!(store.get_push_pseudonym(&ipk), Some([9u8; 32]));
    }

    #[test]
    fn presence_consent_rejects_replayed_version() {
        let store = fresh_store();
        let now = now_ms();
        assert!(store.put_presence_consent(&consent(7, now, true)).unwrap());
        assert!(store.has_presence_consent(&[1u8; 32], &[2u8; 32]));

        assert!(!store.put_presence_consent(&consent(6, now, true)).unwrap());
        assert!(!store.put_presence_consent(&consent(7, now, true)).unwrap());
        assert!(store.put_presence_consent(&consent(8, now, false)).unwrap());
        assert!(!store.has_presence_consent(&[1u8; 32], &[2u8; 32]));
        assert!(!store.put_presence_consent(&consent(8, now, true)).unwrap());
        assert!(!store.has_presence_consent(&[1u8; 32], &[2u8; 32]));
    }

    #[test]
    fn presence_state_rejects_version_beyond_observed_lead() {
        let store = fresh_store();
        let now = now_ms();
        assert!(
            !store
                .put_presence_state(&[1u8; 32], &[2u8; 32], &PresenceState::Online, u64::MAX, now)
                .unwrap()
        );
        assert_eq!(store.get_presence_state(&[1u8; 32], &[2u8; 32]), None);
    }

    #[test]
    fn presence_state_row_is_replaceable_once_stale() {
        let store = fresh_store();
        let t0 = 1_700_000_000_000;
        let high = t0 + PRESENCE_VERSION_MAX_LEAD_MS;
        assert!(
            store
                .put_presence_state(&[1u8; 32], &[2u8; 32], &PresenceState::Online, high, t0)
                .unwrap()
        );
        assert!(
            !store
                .put_presence_state(&[1u8; 32], &[2u8; 32], &PresenceState::Online, high, t0 + 1)
                .unwrap(),
            "a fresh row still wins on version"
        );

        let later = t0 + PRESENCE_STATE_TTL_MS + 1;
        assert!(
            store
                .put_presence_state(&[1u8; 32], &[2u8; 32], &PresenceState::Online, 1, later)
                .unwrap(),
            "a stale row is treated as absent"
        );
    }

    #[test]
    fn sweep_removes_only_expired_rows() {
        let store = fresh_store();
        let now = 10 * IDLE_IDENTITY_TTL_MS;
        store.put_last_seen(&[1u8; 32], now).unwrap();
        store.put_last_seen(&[2u8; 32], now - IDLE_IDENTITY_TTL_MS - 1).unwrap();

        let mut target = SweepTarget::new(&store.last_seen, last_seen_expired);
        sweep(&mut target, now);

        assert_eq!(store.get_last_seen(&[1u8; 32]), Some(now));
        assert_eq!(store.get_last_seen(&[2u8; 32]), None);
    }

    #[test]
    fn sweep_resumes_from_cursor_until_keyspace_is_drained() {
        let store = fresh_store();
        let now = 10 * IDLE_IDENTITY_TTL_MS;
        let stale = now - IDLE_IDENTITY_TTL_MS - 1;
        let rows = MAX_SWEEP_REMOVALS + 32;
        for i in 0..rows {
            let mut ipk = [0u8; 32];
            ipk[..8].copy_from_slice(&(i as u64).to_be_bytes());
            store.put_last_seen(&ipk, stale).unwrap();
        }

        let mut target = SweepTarget::new(&store.last_seen, last_seen_expired);
        sweep(&mut target, now);
        assert!(target.cursor.is_some(), "budget exhausted mid-keyspace");
        assert_eq!(store.last_seen.iter().count(), rows - MAX_SWEEP_REMOVALS);

        sweep(&mut target, now);
        assert!(target.cursor.is_none());
        assert_eq!(store.last_seen.iter().count(), 0);
    }
}
