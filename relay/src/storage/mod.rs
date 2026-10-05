pub mod db;
pub mod key_packages;
pub mod queue;

use common::proto::client_rel::DispatchP;
use common::proto::client_rel::Wake;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use serde::Deserialize;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Past this a recipient's new dispatches get `QueueFull`, bounding disk and drain time per user.
pub const MAX_QUEUED_PER_RECIPIENT: usize = 16_384;

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

/// A row of either queue keyspace. postcard is positional, so a row an older relay wrote needs its
/// own shape: one from before `ttl_ms` was appended, or in `messages` the delivered form, which
/// has no `wake` and no `to` (the key's recipient).
pub fn queued_dispatch(recipient: &[u8; 32], value: &[u8]) -> Option<DispatchP> {
    if let Ok(dispatch) = DispatchP::deser(value) {
        return Some(dispatch);
    }
    if let Ok(UntimedDispatch { to, from, id, payload, sig, accepted_at_ms, wake }) =
        UntimedDispatch::deser(value)
    {
        return Some(DispatchP { to, from, id, payload, sig, accepted_at_ms, wake, ttl_ms: 0 });
    }
    let (Delivered { id, from, payload, sig, accepted_at_ms }, ttl_ms) =
        <(Delivered, u64)>::deser(value)
            .or_else(|_| Delivered::deser(value).map(|delivered| (delivered, 0)))
            .ok()?;
    let to = Bytes(*recipient);
    Some(DispatchP { to, from, id, payload, sig, accepted_at_ms, wake: Wake::No, ttl_ms })
}

/// Its `wake` was a bool, which encodes as `Wake::No` or `Wake::Message`.
#[derive(Deserialize)]
struct UntimedDispatch {
    to:             Bytes<32>,
    from:           Bytes<32>,
    id:             Bytes<16>,
    payload:        ByteVec,
    sig:            Bytes<64>,
    accepted_at_ms: u64,
    wake:           Wake,
}

/// Later rows append `ttl_ms`.
#[derive(Deserialize)]
struct Delivered {
    id:             Bytes<16>,
    from:           Bytes<32>,
    payload:        ByteVec,
    sig:            Bytes<64>,
    accepted_at_ms: u64,
}
