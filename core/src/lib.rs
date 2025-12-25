#![feature(ip_as_octets)]

use std::sync::OnceLock;

use jni::JNIEnv;
use jni::JavaVM;
use jni::objects::JClass;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use quinn::Endpoint;
use tokio::runtime::Runtime;

mod api;
mod data;
mod db;
mod events;
mod identity;
mod quic;
mod utils;

type JE<'local> = JNIEnv<'local>;
type JC<'local> = JClass<'local>;

//////////////////////////////////////////////
//============ GLOBAL VARIABLES ============//
//////////////////////////////////////////////
static JVM: OnceLock<JavaVM> = OnceLock::new();

/// App's Package Name
static PACKAGE_NAME: &str = "com.promtuz.chat";

/// Global Tokio Runtime
pub static RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

pub static ENDPOINT: OnceCell<Endpoint> = OnceCell::new();

//////////////////////////////////////////////
//============ GLOBAL FUNCTIONS ============//
//////////////////////////////////////////////

#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut std::ffi::c_void) -> jni::sys::jint {
    JVM.set(vm).unwrap();
    jni::sys::JNI_VERSION_1_6
}
