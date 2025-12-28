use std::sync::Arc;
use std::time::Duration;

use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca_bytes;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use jni::JNIEnv;
use jni::objects::JObject;
use jni_macro::jni;
use log::info;
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TransportConfig;
use quinn::default_runtime;

use crate::ENDPOINT;
use crate::JC;
use crate::RUNTIME;
use crate::data::identity::Identity;
use crate::jni_try;
use crate::ndk::read_raw_res;

pub mod conn_stats;
pub mod connection;
pub mod identity;
pub mod misc;
pub mod welcome;

#[macro_export]
macro_rules! endpoint {
    () => {
        if let Some(ep) = $crate::ENDPOINT.get() {
            ep
        } else {
            log::error!("API is not initialized.");
            return;
        }
    };
}

/// Entry point for API
///
/// Initializes Endpoint
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn initApi(mut env: JNIEnv, _: JC, context: JObject) {
    info!("API: INIT START");

    let rt = RUNTIME.handle().clone();
    let _guard = rt.enter();

    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();

    let mut endpoint =
        Endpoint::new(EndpointConfig::default(), None, socket, default_runtime().unwrap()).unwrap();

    if let Ok(addr) = endpoint.local_addr() {
        info!("API: ENDPOINT BIND TO {}", addr);
    }

    jni_try!(setup_crypto_provider());

    let root_ca_bytes = jni_try!(read_raw_res(&mut env, &context, "root_ca"));
    let roots = jni_try!(load_root_ca_bytes(&root_ca_bytes));

    let mut client_cfg = jni_try!(build_client_cfg(ProtoRole::Client, &roots));

    let mut transport_cfg = TransportConfig::default();
    transport_cfg.keep_alive_interval(Some(Duration::from_secs(15)));

    client_cfg.transport_config(Arc::new(transport_cfg));

    endpoint.set_default_client_config(client_cfg);

    ENDPOINT.set(endpoint).expect("init was ran twice");
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn shouldLaunchApp(_: JNIEnv, _: JC) -> bool {
    Identity::public_key().is_ok()
}
