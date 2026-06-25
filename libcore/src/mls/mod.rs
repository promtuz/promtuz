//! MLS (RFC 9420) layer — see `misc/specs/MLS.md`.
//!
//! Cipher suite: `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519`
//! (suite ID `0x0003`).
//!
//! # Module layout
//!
//! - `provider.rs`, `storage.rs`, `types.rs`: `PromtuzMlsProvider`
//!   (the openmls `OpenMlsProvider`), the rusqlite-backed
//!   `PromtuzStorageProvider`, and the storage error enum.
//! - `common/src/proto/mls_wire.rs`: wire types
//!   (`MlsApplicationEnvelopeP`, `WelcomeEnvelopeP`,
//!   `KeyPackagePublishReq` etc.) and signing-input helpers.
//! - `signer.rs`, `keypackage.rs`, `relay/src/dht/mls_kp.rs`,
//!   `relay/src/dht/mls_welcome.rs`: leaf-signer adapter, KeyPackage
//!   stash + Welcome queue handlers.
//! - `group.rs`, `welcome.rs`, `epoch_catchup.rs`: the high-level
//!   group runtime (`MlsGroupHandle`), Welcome envelope handling
//!   (`process_welcome`, `make_welcome_envelope`), and the
//!   out-of-order epoch buffer (`EpochCatchupBuffer`).
//! - `libcore/src/api/messaging.rs`: wires MLS into the messaging
//!   path.

pub mod epoch_catchup;
pub mod group;
pub mod keypackage;
pub mod provider;
pub mod scheduler;
pub mod signer;
pub mod storage;
pub mod types;
pub mod welcome;

// Re-export the public surface for downstream consumption. Lint
// allows are required because the cdylib compiler can't see external
// use across the JNI boundary.
#[allow(unused_imports)]
pub use epoch_catchup::{EpochCatchupBuffer, PushOutcome};
#[allow(unused_imports)]
pub use group::{MlsGroupHandle, PROMTUZ_CIPHERSUITE};
#[allow(unused_imports)]
pub use keypackage::{KeyPackageStash, KeyPackageStashError};
#[allow(unused_imports)]
pub use provider::PromtuzMlsProvider;
#[allow(unused_imports)]
pub use signer::Ed25519Signer;
#[allow(unused_imports)]
pub use storage::PromtuzStorageProvider;
#[allow(unused_imports)]
pub use types::{MlsGroupError, PromtuzMlsStorageError};
#[allow(unused_imports)]
pub use welcome::{make_welcome_envelope, process_welcome};

/// Per-`group_id` ceiling on cumulative `mls_storage.value` bytes,
/// before a write is rejected with
/// [`PromtuzMlsStorageError::BudgetExceeded`]. Spec §0, §9.3.
///
/// The cap protects against a malformed or runaway group (e.g. an
/// Add chain crossing `MAX_GROUP_SIZE = 256`, which is capped
/// explicitly elsewhere).
pub const MLS_GROUP_STATE_BUDGET_BYTES: u64 = 1024 * 1024;

/// Per-group cap on application messages held for future epochs.
/// Spec §0 (`MAX_EPOCH_AHEAD_BUFFER`), §7.3.
#[allow(dead_code)] // messaging.rs caller.
pub const MAX_EPOCH_AHEAD_BUFFER: usize = 512;
