use std::{sync::Arc, time::Duration};

use common::quic::{config::{build_client_cfg, load_root_ca_bytes, setup_crypto_provider}, protorole::ProtoRole};
use jni::{JNIEnv, objects::JObject};
use log::info;
use jni_macro::jni;
use quinn::{Endpoint, EndpointConfig, TransportConfig, default_runtime};

use crate::{ENDPOINT, JC, RUNTIME, db::initial_execute, jni_try, utils::ujni::read_raw_res};

pub mod conn_stats;
pub mod connection;
pub mod misc;

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
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Trace).with_tag("core"),
    );
    
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

    //==||==||==||==||==||==||==||==||==||==||==||==||==//
    info!("DB: STARTING SQLITE DATABASE");

    let db_block = (|| {
        info!("DB: INITIALIZING TABLES");
        initial_execute()?;

        Ok::<(), anyhow::Error>(())
    })();

    jni_try!(db_block);
}