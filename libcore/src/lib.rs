use std::sync::Arc;

use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use quinn::Endpoint;
use tokio::runtime::Runtime;

pub mod api;
pub mod data;
pub mod db;
pub mod delivery;
pub mod events;
pub mod messaging;
pub mod mls;
pub mod platform;
pub mod push;
pub mod quic;
pub mod state;
pub mod utils;

uniffi::setup_scaffolding!();

//////////////////////////////////////////////
//============ GLOBAL VARIABLES ============//
//////////////////////////////////////////////

/// Global Tokio Runtime
pub static RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

pub static ENDPOINT: OnceCell<Arc<Endpoint>> = OnceCell::new();

/// Resolver seeds captured at `init`, for ad-hoc lookups outside the relay
/// loop (e.g. discovering a push gateway to register `P → token`).
pub static RESOLVER_SEEDS: OnceCell<Vec<crate::data::ResolverSeed>> = OnceCell::new();
