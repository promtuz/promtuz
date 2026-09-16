use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::bail;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::PushProvider;
use common::proto::push::RegisterToken;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

const MAX_REGISTRATIONS: usize = 100_000;
const REGISTRATION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const EVICT_BATCH: usize = MAX_REGISTRATIONS / 10;

#[derive(Debug, Clone)]
pub struct TokenEntry {
    pub provider: PushProvider,
    pub token:    Vec<u8>,
}

/// Durable pseudonym-to-token bindings. A gateway restart must not leave
/// sleeping devices unreachable until they next open the app.
pub struct PushRegistry {
    db: Mutex<Connection>,
}

impl PushRegistry {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    fn from_connection(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS push_tokens (
                 pseudonym BLOB PRIMARY KEY CHECK(length(pseudonym) = 32),
                 registration BLOB NOT NULL,
                 refreshed_at INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS push_tokens_age ON push_tokens(refreshed_at);",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn register(&self, reg: &RegisterToken) -> Result<()> {
        self.register_at(reg, now())
    }

    fn register_at(&self, reg: &RegisterToken, now: u64) -> Result<()> {
        if !reg.verify() {
            bail!("bad registration signature");
        }
        let bytes = reg.ser()?;
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM push_tokens WHERE pseudonym = ?1)",
            [reg.pseudonym.0.as_slice()],
            |r| r.get(0),
        )?;
        if !exists {
            let count: usize =
                tx.query_row("SELECT COUNT(*) FROM push_tokens", [], |r| r.get(0))?;
            if count >= MAX_REGISTRATIONS {
                tx.execute(
                    "DELETE FROM push_tokens WHERE refreshed_at <= ?1",
                    [now.saturating_sub(REGISTRATION_TTL.as_secs())],
                )?;
                let count: usize =
                    tx.query_row("SELECT COUNT(*) FROM push_tokens", [], |r| r.get(0))?;
                if count >= MAX_REGISTRATIONS {
                    tx.execute(
                        "DELETE FROM push_tokens WHERE pseudonym IN
                         (SELECT pseudonym FROM push_tokens ORDER BY refreshed_at LIMIT ?1)",
                        [EVICT_BATCH],
                    )?;
                }
            }
        }
        tx.execute(
            "INSERT INTO push_tokens (pseudonym, registration, refreshed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(pseudonym) DO UPDATE SET
                 registration = excluded.registration, refreshed_at = excluded.refreshed_at",
            (reg.pseudonym.0.as_slice(), bytes, now),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve(&self, pseudonym: &[u8; 32]) -> Result<Option<TokenEntry>> {
        self.resolve_at(pseudonym, now())
    }

    fn resolve_at(&self, pseudonym: &[u8; 32], now: u64) -> Result<Option<TokenEntry>> {
        let bytes: Option<Vec<u8>> = self
            .db
            .lock()
            .query_row(
                "SELECT registration FROM push_tokens WHERE pseudonym = ?1 AND refreshed_at > ?2",
                (pseudonym.as_slice(), now.saturating_sub(REGISTRATION_TTL.as_secs())),
                |r| r.get(0),
            )
            .optional()?;
        bytes
            .map(|bytes| {
                let reg = RegisterToken::deser(&bytes)?;
                Ok(TokenEntry { provider: reg.provider, token: reg.token })
            })
            .transpose()
    }

    pub fn sweep(&self) -> Result<()> {
        self.sweep_at(now())
    }

    fn sweep_at(&self, now: u64) -> Result<()> {
        self.db.lock().execute(
            "DELETE FROM push_tokens WHERE refreshed_at <= ?1",
            [now.saturating_sub(REGISTRATION_TTL.as_secs())],
        )?;
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn registration(token: &[u8]) -> RegisterToken {
        RegisterToken::signed(&SigningKey::from_bytes(&[7; 32]), PushProvider::Fcm, token.to_vec())
    }

    #[test]
    fn registrations_and_token_rotation_survive_restart() {
        let path = std::env::temp_dir().join(format!("pz-push-{}.db", rand_suffix()));
        let first = registration(b"first");
        {
            let registry = PushRegistry::open(&path).unwrap();
            registry.register(&first).unwrap();
        }
        {
            let registry = PushRegistry::open(&path).unwrap();
            assert_eq!(registry.resolve(&first.pseudonym.0).unwrap().unwrap().token, b"first");
            registry.register(&registration(b"rotated")).unwrap();
        }
        {
            let registry = PushRegistry::open(&path).unwrap();
            assert_eq!(registry.resolve(&first.pseudonym.0).unwrap().unwrap().token, b"rotated");
            let mut forged = registration(b"forged");
            forged.token = b"tampered".to_vec();
            assert!(registry.register(&forged).is_err());
            assert_eq!(registry.resolve(&first.pseudonym.0).unwrap().unwrap().token, b"rotated");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn expiry_is_preserved_across_restart() {
        let path = std::env::temp_dir().join(format!("pz-push-{}.db", rand_suffix()));
        let reg = registration(b"token");
        let start = 100;
        {
            let registry = PushRegistry::open(&path).unwrap();
            registry.register_at(&reg, start).unwrap();
        }
        {
            let registry = PushRegistry::open(&path).unwrap();
            let expiry = start + REGISTRATION_TTL.as_secs();
            assert!(registry.resolve_at(&reg.pseudonym.0, expiry - 1).unwrap().is_some());
            assert!(registry.resolve_at(&reg.pseudonym.0, expiry).unwrap().is_none());
            registry.sweep_at(expiry).unwrap();
            assert_eq!(
                registry
                    .db
                    .lock()
                    .query_row("SELECT COUNT(*) FROM push_tokens", [], |r| r.get::<_, usize>(0))
                    .unwrap(),
                0
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    fn rand_suffix() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )
    }
}
