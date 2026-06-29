use std::sync::Arc;

use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use quinn::Endpoint;
use tokio::runtime::Runtime;

// These modules are `pub` so the e2e harness (and the relay's integration
// tests) can drive them via the rlib. The cdylib FFI build (uniffi) is
// unaffected; crate-type stays ["cdylib", "rlib"].
pub mod api;
pub mod data;
pub mod db;
pub mod events;
pub mod messaging;
pub mod mls;
pub mod platform;
pub mod quic;
pub mod state;
pub mod utils;

/// Headless end-to-end client driver (feature `e2e-client`). Drives the
/// real MLS + [`crate::quic::relay_dht_client::RelayDhtClient`] pipeline
/// over a live `client/0` connection with explicit keys — no keystore, no
/// global state — for the `testnet` sandbox harness. Compiled out of the
/// normal cdylib build.
#[cfg(feature = "e2e-client")]
pub mod e2e;

uniffi::setup_scaffolding!();

//////////////////////////////////////////////
//============ GLOBAL VARIABLES ============//
//////////////////////////////////////////////

/// Global Tokio Runtime
pub static RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

pub static ENDPOINT: OnceCell<Arc<Endpoint>> = OnceCell::new();
