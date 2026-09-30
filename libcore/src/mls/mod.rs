//! MLS (RFC 9420) layer.
//!
//! Cipher suite: `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519`
//! (suite ID `0x0003`).
//!
//! # Module layout
//!
//! - `provider.rs`, `storage.rs`, `types.rs`: `PromtuzMlsProvider` (the openmls `OpenMlsProvider`),
//!   the rusqlite-backed `PromtuzStorageProvider`, and the storage error enum.
//! - `common/src/proto/mls_wire.rs`: wire types (`MlsApplicationEnvelopeP`, `WelcomeEnvelopeP`,
//!   `KeyPackagePublishReq` etc.) and signing-input helpers.
//! - `signer.rs`, `keypackage.rs`, `relay/src/dht/mls_kp.rs`, `relay/src/dht/mls_welcome.rs`:
//!   leaf-signer adapter, KeyPackage stash + Welcome queue handlers.
//! - `group.rs`, `welcome.rs`, `epoch_catchup.rs`: the high-level group runtime (`MlsGroupHandle`),
//!   Welcome envelope handling (`process_welcome`, `make_welcome_envelope`), and the out-of-order
//!   epoch buffer (`EpochCatchupBuffer`).
//! - `libcore/src/api/messaging.rs`: wires MLS into the messaging path.

pub(crate) mod branch_proof;
pub mod credential;
pub mod epoch_catchup;
pub mod group;
pub mod keypackage;
pub(crate) mod migration;
pub mod policy;
pub mod provider;
pub(crate) mod recovery;
pub mod scheduler;
pub mod signer;
pub mod storage;
pub mod types;
pub mod welcome;

// Re-export the public surface for downstream consumption. Lint
// allows are required because the cdylib compiler can't see external
// use across the JNI boundary.
#[allow(unused_imports)]
pub use epoch_catchup::EpochCatchupBuffer;
#[allow(unused_imports)]
pub use epoch_catchup::PushOutcome;
#[allow(unused_imports)]
pub use group::Changed;
#[allow(unused_imports)]
pub use group::CommitOutcome;
#[allow(unused_imports)]
pub use group::GROUP_META_EXTENSION;
#[allow(unused_imports)]
pub use group::GroupMeta;
#[allow(unused_imports)]
pub use group::MlsGroupHandle;
#[allow(unused_imports)]
pub use group::PROMTUZ_CIPHERSUITE;
#[allow(unused_imports)]
pub use keypackage::KeyPackageStash;
#[allow(unused_imports)]
pub use keypackage::KeyPackageStashError;
#[allow(unused_imports)]
pub use policy::GroupState;
#[allow(unused_imports)]
pub use provider::PromtuzMlsProvider;
#[allow(unused_imports)]
pub use signer::Ed25519Signer;
#[allow(unused_imports)]
pub use storage::PromtuzStorageProvider;
#[allow(unused_imports)]
pub use types::MlsGroupError;
#[allow(unused_imports)]
pub use types::PromtuzMlsStorageError;
#[allow(unused_imports)]
pub use welcome::encode_welcome;
#[allow(unused_imports)]
pub use welcome::make_welcome_envelope;
#[allow(unused_imports)]
pub use welcome::process_welcome;
#[allow(unused_imports)]
pub use welcome::seal_welcome_blob;

/// Per-`group_id` ceiling on cumulative `mls_storage.value` bytes,
/// before a write is rejected with
/// [`PromtuzMlsStorageError::BudgetExceeded`].
///
/// The last line of defence against a runaway Add chain; [`MAX_GROUP_MEMBERS`]
/// is the one that should reject it first.
pub const MLS_GROUP_STATE_BUDGET_BYTES: u64 = 1024 * 1024;

/// Ceiling on members in a group, enforced when joining via a Welcome and when
/// merging a staged commit.
pub const MAX_GROUP_MEMBERS: usize = 256;

/// Application plaintext is padded up to a multiple of this before sealing, so
/// ciphertext length reports a bucket rather than the message length. Applied
/// by the sender; openmls strips it on decrypt, so it needs no wire change.
pub const MLS_PADDING_SIZE: usize = 256;

/// Per-group cap on application messages held for future epochs.
#[allow(dead_code)] // messaging.rs caller.
pub const MAX_EPOCH_AHEAD_BUFFER: usize = 512;

/// Per-group cap on bytes held for future epochs. Binds before
/// [`MAX_EPOCH_AHEAD_BUFFER`] whenever buffered messages are large.
#[allow(dead_code)] // messaging.rs caller.
pub const MAX_EPOCH_AHEAD_BYTES: u64 = 8 * 1024 * 1024;
