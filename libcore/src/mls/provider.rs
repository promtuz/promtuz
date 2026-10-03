//! The openmls provider: one `RustCrypto` for crypto and randomness, and the SQLite storage.

use std::sync::Arc;

use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;
use parking_lot::Mutex;
use rusqlite::Connection;

use super::storage::PromtuzStorageProvider;

use crate::state::core;

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
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            crypto: RustCrypto::default(),
            storage: PromtuzStorageProvider::new(conn),
        }
    }

    /// The provider over the core's MLS connection. Its mutex guards that one connection;
    /// `recovery::operation_lock` is what serializes a group's load and use.
    pub fn shared() -> Self {
        Self::new(core().db.mls())
    }

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
