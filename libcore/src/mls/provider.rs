//! `PromtuzMlsProvider`: the `OpenMlsProvider` implementation used by
//! every libcore-side openmls call site.
//!
//! Composition (see `misc/specs/MLS.md` §9):
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

    /// Build a provider that points at the on-disk libcore singleton
    /// (`db("mls")` path). Convenience for production code paths.
    #[allow(dead_code)] // Consumed by the group lifecycle wiring.
    pub fn shared() -> Self {
        // Borrow the static handle's `Arc<Mutex<_>>` via a thin wrapper:
        // we want a clone-able owning handle, and `Lazy<Mutex<…>>` gives
        // a `&Mutex<…>`. Promote it by wrapping in an `Arc` over a *new*
        // mutex would defeat sharing — instead we wrap the *static* in
        // an Arc-of-reference via a custom guard. Simpler: re-open the
        // SQLite file. SQLite (with WAL) handles multiple connections
        // to the same file fine.
        //
        // We open a fresh connection here so each `shared()` call gets
        // its own physical handle — this matches the pattern used by
        // `IDENTITY_DB`/`MESSAGES_DB` (which are themselves single
        // connections, but the rest of libcore opens separate ones if
        // it wants concurrency).
        let path = crate::db::db("mls");
        let mut conn = Connection::open(path).expect("open mls db");
        crate::db::mls::apply_mls_migrations(&mut conn);
        Self::new(Arc::new(Mutex::new(conn)))
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
    fn construction_smoke() {
        let p = build_provider();
        // Touch every accessor — no panic, no compile-error on the
        // OpenMlsProvider trait.
        let _ = OpenMlsProvider::storage(&p);
        let _ = OpenMlsProvider::crypto(&p);
        let _ = OpenMlsProvider::rand(&p);
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
