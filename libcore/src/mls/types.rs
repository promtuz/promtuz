//! MLS error types.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PromtuzMlsStorageError {
    #[error("rusqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("CBOR encode failed: {0}")]
    Encode(String),

    #[error("CBOR decode failed: {0}")]
    Decode(String),

    /// Refused before anything is written.
    #[error(
        "MLS group storage budget exceeded: group needs {requested} B, \
         existing {existing} B, limit {limit} B"
    )]
    BudgetExceeded {
        existing: u64,
        requested: u64,
        limit: u64,
    },

    #[error("a storage operation is already open on this thread")]
    Nested,
}

impl PromtuzMlsStorageError {
    pub(crate) fn encode<E: std::fmt::Display>(e: E) -> Self {
        Self::Encode(e.to_string())
    }
    pub(crate) fn decode<E: std::fmt::Display>(e: E) -> Self {
        Self::Decode(e.to_string())
    }
}

/// openmls errors travel as their `Debug` text, since each openmls error type is generic over the
/// storage error.
#[derive(Error, Debug)]
pub enum MlsGroupError {
    #[error("storage: {0}")]
    Storage(#[from] PromtuzMlsStorageError),

    #[error("openmls: {0}")]
    OpenMls(String),

    #[error("envelope signature failed verification")]
    BadSignature,

    /// The sending leaf proves no identity. Receivers ack and drop the message rather than retry:
    /// waiting will not bind the leaf.
    #[error("message from a leaf bound to no identity")]
    UnboundSender,

    #[error("cipher suite mismatch (expected MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519)")]
    BadCipherSuite,

    #[error("tls_codec: {0}")]
    Codec(String),

    #[error("internal invariant violated: {0}")]
    Internal(String),
}

impl From<rusqlite::Error> for MlsGroupError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

impl MlsGroupError {
    /// A consumed or expired sender-ratchet secret cannot be recovered.
    pub(crate) fn is_spent_secret(&self) -> bool {
        matches!(
            self,
            Self::OpenMls(reason)
                if reason.contains("TooDistantInThePast") || reason.contains("SecretReuseError")
        )
    }

    pub(crate) fn from_openmls<E: std::fmt::Debug>(e: E) -> Self {
        Self::OpenMls(format!("{e:?}"))
    }

    pub(crate) fn from_codec<E: std::fmt::Display>(e: E) -> Self {
        Self::Codec(e.to_string())
    }
}
