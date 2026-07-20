//! P2P attachment transfer: chunked-manifest protocol for files too big for
//! the inline `Image` message (>256KB), carried over a direct link from
//! [`crate::p2p`] rather than the store-and-forward relay.

pub mod wire;
