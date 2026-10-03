pub mod db;
pub mod key_packages;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Past this a recipient's new dispatches get `QueueFull`, bounding disk and drain time per user.
pub const MAX_QUEUED_PER_RECIPIENT: usize = 1024;

/// The recipient stays at offset 0 so a 32-byte prefix scan yields one user's queue; the
/// big-endian ms timestamp keeps it in order, and the client's message id makes the key unique.
#[derive(Debug, Clone, Copy, KnownLayout, FromBytes, IntoBytes, Immutable)]
#[repr(C, packed)]
pub struct MessageKey {
    pub recipient: [u8; 32],
    pub ts_be:     [u8; 8],
    pub id:        [u8; 16],
}

impl MessageKey {
    pub const SIZE: usize = 56;

    pub fn new(recipient: &[u8; 32], ts_ms: u64, id: &[u8; 16]) -> Self {
        Self { recipient: *recipient, ts_be: ts_ms.to_be_bytes(), id: *id }
    }

    pub fn as_bytes(&self) -> &[u8] {
        IntoBytes::as_bytes(self)
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        Self::read_from_bytes(bytes).ok()
    }
}
