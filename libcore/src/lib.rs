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

mod api;
mod data;
mod db;
mod events;
mod ndk;
mod quic;
mod utils;

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
