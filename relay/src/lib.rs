//! Relay library surface — re-exports of the crate's modules so other
//! crates' integration tests (notably the libcore e2e harness) can
//! drive `Dht::new`, the `peer/1` handler, and the fjall `Store`
//! directly.
//!
//! # Why this exists
//!
//! The relay is primarily a binary (see `bin/main.rs` and `main.rs`).
//! The e2e tests need to spin up real `Dht` instances inside a test
//! harness, which requires the crate to also be linkable as a library.
//! Visibility on individual modules stays `pub(crate)` by default —
//! only what the e2e harness consumes is bumped to `pub`, and that's
//! documented in the modules themselves.
//!
//! No new code lives in this file. Behaviour changes happen in the
//! underlying modules (`dht`, `quic`, etc.); this is purely a façade.

pub mod dht;
pub mod quic;
pub mod relay;
pub mod storage;
pub mod util;
