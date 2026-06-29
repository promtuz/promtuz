pub mod db;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Per-recipient queue cap. Past this, new dispatches for the recipient are
/// rejected with `DispatchAckP::QueueFull` and the sender is expected to back
/// off rather than continuing to push.
///
/// Why a cap at all: a single sender can otherwise queue millions of messages
/// for an offline recipient, exhausting disk and ballooning per-recipient
/// drain time. 1024 is a rough ceiling on a per-user backlog before a drain
/// starts becoming expensive — at ~4 KiB per `DeliverP` that's ~4 MiB worst
/// case per offline user. Operators can revisit this once we have better
/// telemetry on real backlogs.
#[allow(dead_code)]
pub const MAX_QUEUED_PER_RECIPIENT: usize = 1024;

/// On-disk fjall key for a queued message.
///
/// Layout (`#[repr(C, packed)]`):
/// - `recipient: [u8; 32]` — recipient IPK; must remain at offset 0 so a
///   32-byte `prefix()` scan groups all messages for a single user.
/// - `ts_be: [u8; 8]`      — millisecond timestamp, big-endian (sortable).
/// - `id:    [u8; 16]`     — message id (UUIDv7) supplied by the *client*.
///   This replaces the old random suffix to deduplicate the key without
///   relying on birthday-prone random bytes.
///
/// Total size: 56 bytes. Construct with [`MessageKey::new`] and use
/// [`MessageKey::as_bytes`] / [`MessageKey::parse`] for IO.
#[derive(Debug, Clone, Copy, KnownLayout, FromBytes, IntoBytes, Immutable)]
#[repr(C, packed)]
pub struct MessageKey {
    pub recipient: [u8; 32],
    pub ts_be:     [u8; 8],
    pub id:        [u8; 16],
}

impl MessageKey {
    pub const SIZE: usize = 56;

    #[allow(dead_code)] // not used by every binary that includes this module
    pub fn new(recipient: &[u8; 32], ts_ms: u64, id: &[u8; 16]) -> Self {
        Self { recipient: *recipient, ts_be: ts_ms.to_be_bytes(), id: *id }
    }

    #[allow(dead_code)] // not used by every binary that includes this module
    pub fn as_bytes(&self) -> &[u8; Self::SIZE] {
        // SAFETY: `MessageKey` is `#[repr(C, packed)]` with three byte arrays
        // totalling exactly 56 bytes; the field layout has no padding, so the
        // reinterpret-cast is sound.
        unsafe { &*(self as *const Self as *const [u8; Self::SIZE]) }
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        Self::read_from_bytes(bytes).ok()
    }
}
