//! Promtuz MLS shared types and error definitions.
//!
//! [`PromtuzMlsStorageError`] is the storage-side error.
//! [`MlsGroupError`] is the error type surfaced by the
//! `MlsGroupHandle`, Welcome processing, and epoch-catchup paths.

use thiserror::Error;

/// Errors raised by `PromtuzStorageProvider`.
///
/// The variants intentionally cover only the failure modes openmls is
/// expected to surface. Anything genuinely unexpected (e.g. a poisoned
/// mutex on the rusqlite connection) is folded into [`Self::Encode`] /
/// [`Self::Decode`] / [`Self::Sqlite`] with the underlying message
/// preserved.
#[derive(Error, Debug)]
pub enum PromtuzMlsStorageError {
    /// Underlying SQLite returned an error during a write/read/delete.
    #[error("rusqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A serde-encoded value (key or entity) failed to encode to CBOR.
    #[error("CBOR encode failed: {0}")]
    Encode(String),

    /// A stored CBOR blob failed to decode (corruption or schema drift).
    #[error("CBOR decode failed: {0}")]
    Decode(String),

    /// Writing this entity would push the per-`group_id` storage past
    /// `MLS_GROUP_STATE_BUDGET_BYTES` (1 MiB). The write is *rejected*;
    /// no partial write occurs.
    #[error(
        "MLS group storage budget exceeded: group needs {requested} B, \
         existing {existing} B, limit {limit} B"
    )]
    BudgetExceeded {
        existing: u64,
        requested: u64,
        limit: u64,
    },
}

impl PromtuzMlsStorageError {
    pub(crate) fn encode<E: std::fmt::Display>(e: E) -> Self {
        Self::Encode(e.to_string())
    }
    pub(crate) fn decode<E: std::fmt::Display>(e: E) -> Self {
        Self::Decode(e.to_string())
    }
}

/// Errors surfaced by the high-level `MlsGroupHandle` /
/// `process_welcome` / `EpochCatchupBuffer` APIs.
///
/// **Variants are stringly-typed for openmls failures** because every
/// openmls error type is itself parameterised over the storage error
/// (`AddMembersError<StorageError>`, `ProcessMessageError<StorageError>`,
/// …). Trying to enumerate them in a single `From` impl would be a
/// combinatorial explosion. We bridge with `Display` instead — the
/// underlying `Debug`/`Display` impls already include enough detail
/// for diagnostics (and tests assert on variant *kind*, not on inner
/// strings).
///
/// `BadCipherSuite`, `EpochAhead`, and `EpochStale` are present in the
/// enum so `messaging.rs` can match on them by variant kind.
#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum MlsGroupError {
    /// Underlying storage error (rusqlite, CBOR codec, budget).
    /// openmls calls funnel storage failures through here.
    #[error("storage: {0}")]
    Storage(#[from] PromtuzMlsStorageError),

    /// Wrapped openmls error. Carries `Debug` rendering so we don't
    /// have to enumerate every parameterised variant.
    #[error("openmls: {0}")]
    OpenMls(String),

    /// Outer envelope signature failed verification (`process_welcome`
    /// rejects before openmls touches the welcome blob).
    #[error("envelope signature failed verification")]
    BadSignature,

    /// Cipher suite mismatch between the wire and our pinned suite
    /// (`MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519`,
    /// `0x0003`).
    #[error("cipher suite mismatch (expected MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519)")]
    BadCipherSuite,

    /// The buffered message belongs to a future epoch beyond the
    /// current group epoch. The caller stashed it in
    /// `mls_epoch_ahead`; this is informational, not an error in the
    /// strict sense — present in the enum so call sites can pattern-
    /// match.
    #[error("message epoch {message} is ahead of current epoch {current}; buffered")]
    EpochAhead { current: u64, message: u64 },

    /// The message is for an epoch we have already left behind and
    /// for which openmls has discarded the message-secrets store.
    /// The recipient should drop the message; not strictly an error,
    /// emitted when openmls itself rejects with WrongEpoch on a feed.
    #[error("message epoch is older than current; dropped")]
    EpochStale,

    /// `tls_codec` (openmls's wire codec) failed to encode/decode an
    /// MLS message. Wrapped because `tls_codec::Error: !Eq`, and
    /// owning the value via `String` keeps the error tree friendly to
    /// callers without manual `Debug` plumbing.
    #[error("tls_codec: {0}")]
    Codec(String),

    /// `postcard` codec failed (our outer envelope encoding).
    #[error("postcard: {0}")]
    Postcard(String),

    /// Internal invariant violated — e.g. caller asked for an epoch
    /// drain on a group that has no `MlsGroup` loaded. Surfaced
    /// rather than panicked because callers cross the JNI boundary.
    #[error("internal invariant violated: {0}")]
    Internal(String),
}

impl MlsGroupError {
    /// A previously consumed or expired sender-ratchet secret cannot be recovered.
    pub(crate) fn is_spent_secret(&self) -> bool {
        matches!(
            self,
            Self::OpenMls(reason)
                if reason.contains("TooDistantInThePast") || reason.contains("SecretReuseError")
        )
    }

    /// Bridge an openmls error (any variant) into the unified enum.
    /// Use the *Debug* rendering rather than Display because
    /// openmls's Display is sometimes terser than the underlying
    /// detail we want for diagnostics.
    #[allow(dead_code)] // messaging.rs caller.
    pub(crate) fn from_openmls<E: std::fmt::Debug>(e: E) -> Self {
        Self::OpenMls(format!("{e:?}"))
    }

    /// Bridge a `tls_codec::Error`.
    #[allow(dead_code)] // messaging.rs caller.
    pub(crate) fn from_codec<E: std::fmt::Display>(e: E) -> Self {
        Self::Codec(e.to_string())
    }

    /// Bridge a `postcard::Error`.
    #[allow(dead_code)] // messaging.rs caller.
    pub(crate) fn from_postcard<E: std::fmt::Display>(e: E) -> Self {
        Self::Postcard(e.to_string())
    }
}
