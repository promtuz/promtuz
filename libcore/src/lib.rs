#![feature(ip_as_octets)]

use std::sync::Arc;
use std::sync::OnceLock;

use jni::JavaVM;
use jni::objects::JClass;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use quinn::Endpoint;
use tokio::runtime::Runtime;

use crate::ndk::key_manager::KeyManager;

// Expose the modules the e2e harness in `relay/tests/` drives. Each
// was `pub(crate)` before and contained no extra-crate callers under
// cdylib (Android only goes through `#[jni]` exports). Bumping to
// `pub` is benign for the JNI build — the crate-type set stays
// `["cdylib", "rlib"]`. Tests link via the rlib.
pub mod api;
pub mod data;
pub mod db;
pub mod events;
pub mod messaging;
pub mod mls;
mod ndk;
pub mod platform;
pub mod quic;
pub mod state;
pub mod utils;

/// Headless end-to-end client driver (feature `e2e-client`). Drives the
/// real MLS + [`crate::quic::relay_dht_client::RelayDhtClient`] pipeline
/// over a live `client/0` connection with explicit keys — no JNI keystore,
/// no global state — for the `testnet` sandbox harness. Compiled out of the
/// normal cdylib build.
#[cfg(feature = "e2e-client")]
pub mod e2e;

uniffi::setup_scaffolding!();

type JC<'local> = JClass<'local>;

//////////////////////////////////////////////
//============ GLOBAL VARIABLES ============//
//////////////////////////////////////////////
static JVM: OnceLock<JavaVM> = OnceLock::new();

/// Global Tokio Runtime
pub static RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

pub static ENDPOINT: OnceCell<Arc<Endpoint>> = OnceCell::new();

pub static KEY_MANAGER: OnceCell<Arc<KeyManager>> = OnceCell::new();

//////////////////////////////////////////////
//============ GLOBAL FUNCTIONS ============//
//////////////////////////////////////////////

#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut std::ffi::c_void) -> jni::sys::jint {
    JVM.set(vm).unwrap();

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("core")
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter(None, log::LevelFilter::Off)
                    .filter_module("core", log::LevelFilter::Debug)
                    .build(),
            ),
    );

    jni::sys::JNI_VERSION_1_6
}
