//! MLS (RFC 9420) layer.

pub(crate) mod branch_proof;
pub mod credential;
pub mod epoch_catchup;
pub mod group;
pub mod keypackage;
pub mod policy;
pub mod provider;
pub(crate) mod recovery;
pub mod scheduler;
pub mod storage;
pub mod types;
pub mod welcome;

#[cfg(test)]
mod recovery_tests;

pub use epoch_catchup::EpochCatchupBuffer;
pub use epoch_catchup::PushOutcome;
pub use group::Changed;
pub use group::CommitOutcome;
pub use group::GROUP_META_EXTENSION;
pub use group::GroupMeta;
pub use group::MlsGroupHandle;
pub use group::PROMTUZ_CIPHERSUITE;
pub use keypackage::KeyPackageStash;
pub use keypackage::KeyPackageStashError;
pub use policy::GroupState;
pub use provider::PromtuzMlsProvider;
pub use storage::PromtuzStorageProvider;
pub use types::MlsGroupError;
pub use types::PromtuzMlsStorageError;
pub use welcome::encode_welcome;
pub use welcome::make_welcome_envelope;
pub use welcome::process_welcome;
pub use welcome::seal_welcome_blob;

/// Per-group ceiling on stored MLS state. The last line of defence against a runaway Add chain;
/// [`MAX_GROUP_MEMBERS`] should refuse it first.
pub const MLS_GROUP_STATE_BUDGET_BYTES: u64 = 1024 * 1024;

/// Enforced when joining from a Welcome and when merging a commit.
pub const MAX_GROUP_MEMBERS: usize = 256;

/// Plaintext is padded to a multiple of this, so ciphertext length reveals only a bucket, not the
/// message length. openmls strips the padding on decrypt.
pub const MLS_PADDING_SIZE: usize = 256;

/// Per-group cap on messages held for future epochs.
pub const MAX_EPOCH_AHEAD_BUFFER: usize = 512;

/// Per-group cap on bytes held for future epochs.
pub const MAX_EPOCH_AHEAD_BYTES: u64 = 8 * 1024 * 1024;
