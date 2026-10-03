use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::PushProvider;
use common::proto::push::RegisterToken;
use common::utils::now_secs;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

const MAX_REGISTRATIONS: usize = 100_000;
const REGISTRATION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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
        self.register_at(reg, now_secs())
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
                    bail!("push registry full");
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
        self.resolve_at(pseudonym, now_secs())
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
        self.sweep_at(now_secs())
    }

    fn sweep_at(&self, now: u64) -> Result<()> {
        self.db.lock().execute(
            "DELETE FROM push_tokens WHERE refreshed_at <= ?1",
            [now.saturating_sub(REGISTRATION_TTL.as_secs())],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::crypto::SigningKey;

    use super::*;

    const TTL: u64 = REGISTRATION_TTL.as_secs();

    fn signed(device: u8, token: &[u8]) -> RegisterToken {
        let key = SigningKey::from_bytes(&[device; 32]);
        RegisterToken::signed(&key, PushProvider::Fcm, token.to_vec())
    }

    fn token(registry: &PushRegistry, reg: &RegisterToken, now: u64) -> Option<Vec<u8>> {
        registry.resolve_at(&reg.pseudonym.0, now).unwrap().map(|entry| entry.token)
    }

    fn rows(registry: &PushRegistry) -> usize {
        registry.db.lock().query_row("SELECT COUNT(*) FROM push_tokens", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_registration_survives_restarts_until_its_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push.db");
        let device = signed(1, b"first");
        let t0 = 1_800_000_000;
        PushRegistry::open(&path).unwrap().register_at(&device, t0).unwrap();

        let registry = PushRegistry::open(&path).unwrap();
        assert_eq!(token(&registry, &device, t0), Some(b"first".to_vec()));
        registry.register_at(&signed(1, b"rotated"), t0 + 10).unwrap();
        let mut forged = signed(1, b"forged");
        forged.token = b"tampered".to_vec();
        assert!(registry.register_at(&forged, t0 + 20).is_err());

        let registry = PushRegistry::open(&path).unwrap();
        let expiry = t0 + 10 + TTL;
        assert_eq!(token(&registry, &device, expiry - 1), Some(b"rotated".to_vec()));
        assert_eq!(token(&registry, &device, expiry), None);
        registry.sweep_at(expiry - 1).unwrap();
        assert_eq!(rows(&registry), 1);
        registry.sweep_at(expiry).unwrap();
        assert_eq!(rows(&registry), 0);
    }

    #[test]
    fn a_full_registry_refuses_newcomers_without_evicting_live_devices() {
        let registry =
            PushRegistry::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let now = 1_800_000_000;
        let device = signed(1, b"device");
        registry.register_at(&device, now).unwrap();
        registry
            .db
            .lock()
            .execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
                 INSERT INTO push_tokens (pseudonym, registration, refreshed_at)
                 SELECT CAST(printf('%032d', i) AS BLOB), x'00', ?2 FROM n",
                (MAX_REGISTRATIONS - 1, now),
            )
            .unwrap();

        assert!(registry.register_at(&signed(2, b"newcomer"), now).is_err());
        assert_eq!(rows(&registry), MAX_REGISTRATIONS);
        registry.register_at(&signed(1, b"rotated"), now + 1).unwrap();
        assert_eq!(token(&registry, &device, now + 1), Some(b"rotated".to_vec()));

        registry
            .db
            .lock()
            .execute(
                "UPDATE push_tokens SET refreshed_at = ?1
                 WHERE pseudonym = CAST(printf('%032d', 1) AS BLOB)",
                [now - TTL],
            )
            .unwrap();
        registry.register_at(&signed(2, b"newcomer"), now).unwrap();
        assert_eq!(rows(&registry), MAX_REGISTRATIONS);
        assert_eq!(token(&registry, &device, now + 1), Some(b"rotated".to_vec()));
    }
}
