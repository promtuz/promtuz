//! `PromtuzMlsProvider`: the `OpenMlsProvider` implementation used by
//! every libcore-side openmls call site.
//!
//! Composition:
//!
//! - **Crypto**: `openmls_rust_crypto::RustCrypto` (stock). All low-level
//!   primitives (Ed25519, X25519, AEAD, hash, HKDF) are exactly the
//!   crates already in the workspace; we don't re-implement.
//! - **Rand**: same `RustCrypto` instance — it implements both
//!   `OpenMlsCrypto` and `OpenMlsRand` over a `ChaCha20Rng` seeded from
//!   `OsRng`. No need for a separate randomness wrapper.
//! - **Storage**: [`PromtuzStorageProvider`] (rusqlite-backed,
//!   per-group budget enforcement, see `storage.rs`).
//!
//! The provider is `Clone`-cheap: storage holds an `Arc<Mutex<…>>`,
//! `RustCrypto` is `Default`-constructed lazily inside if needed —
//! we keep one `RustCrypto` per provider since it owns its own RNG
//! state and there's no benefit to sharing across providers.

use std::sync::Arc;

use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;
use parking_lot::Mutex;
use rusqlite::Connection;

use super::storage::PromtuzStorageProvider;

/// The promtuz `OpenMlsProvider`.
///
/// Group lifecycle, KeyPackage stash, and Welcome handling are built
/// on top of this provider.
pub struct PromtuzMlsProvider {
    crypto: RustCrypto,
    storage: PromtuzStorageProvider,
}

impl std::fmt::Debug for PromtuzMlsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromtuzMlsProvider")
            .field("storage", &self.storage)
            .finish_non_exhaustive()
    }
}

impl PromtuzMlsProvider {
    /// Build a provider from a caller-supplied SQLite connection.
    ///
    /// The connection must already have the MLS schema applied; in
    /// production `db::mls::MLS_DB` does that, in tests use
    /// `db::mls::apply_mls_migrations`.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            crypto: RustCrypto::default(),
            storage: PromtuzStorageProvider::new(conn),
        }
    }

    /// Build a provider over the process-global MLS connection.
    ///
    /// Every production call site goes through here, so they all contend on
    /// one mutex: two concurrent sends must not mutate MLS state through
    /// separate connections, or the second reuses a ratchet secret the first
    /// already consumed.
    pub fn shared() -> Self {
        Self::new(crate::db::mls::stash_db_handle())
    }

    /// Cloneable handle to the storage provider — useful when callers
    /// want to bypass the `OpenMlsProvider` indirection (e.g. promtuz's
    /// own KeyPackage stash table).
    #[allow(dead_code)] // Group lifecycle entrypoint.
    pub fn storage(&self) -> &PromtuzStorageProvider {
        &self.storage
    }
}

impl OpenMlsProvider for PromtuzMlsProvider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = PromtuzStorageProvider;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }
    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }
    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use openmls_traits::random::OpenMlsRand;

    fn build_provider() -> PromtuzMlsProvider {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        PromtuzMlsProvider::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn rand_returns_different_arrays() {
        let p = build_provider();
        let a: [u8; 32] = OpenMlsProvider::rand(&p).random_array().expect("rand");
        let b: [u8; 32] = OpenMlsProvider::rand(&p).random_array().expect("rand");
        // Non-deterministic guard — collisions on 32 random bytes have
        // negligible probability.
        assert_ne!(a, b);
        let v = OpenMlsProvider::rand(&p).random_vec(64).expect("rand vec");
        assert_eq!(v.len(), 64);
    }
}
