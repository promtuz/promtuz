use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca_bytes;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use jni::JNIEnv;
use jni::objects::JObject;
use jni_macro::jni;
use log::info;
use once_cell::sync::OnceCell;
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TransportConfig;
use quinn::default_runtime;

use crate::ENDPOINT;
use crate::JC;
use crate::KEY_MANAGER;
use crate::RUNTIME;
use crate::data::identity::Identity;
use crate::jni_try;
use crate::ndk::key_manager::KeyManager;
use crate::ndk::read_raw_res;
use crate::quic::peer_config::build_peer_server_cfg;
use crate::quic::peer_identity::PeerIdentity;

pub mod conn_stats;
pub mod connection;
pub mod identity;
pub mod messaging;
pub mod misc;
pub mod welcome;

/// Identity used in connecting with peer clients
pub static PEER_IDENTITY: OnceCell<PeerIdentity> = OnceCell::new();

/// Entry point for API
///
/// Initializes Endpoint
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn initApi(mut env: JNIEnv, _: JC, context: JObject) {
    info!("API: INIT START");
    jni_try!(setup_crypto_provider());

    KEY_MANAGER.set(Arc::new(KeyManager::new(&mut env).unwrap())).expect("init was ran twice");

    let rt = RUNTIME.handle().clone();
    let _guard = rt.enter();

    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();

    PeerIdentity::initialize()
        .and_then(|pi| {
            PEER_IDENTITY
                .set(pi)
                .map_err(|_| anyhow!("Failed to set PEER_IDENTITY (already initialized?)"))
        })
        .inspect_err(|e| log::error!("ERROR: failed to initialize peer identity: {e}"))
        .ok();

    let server_cfg = PEER_IDENTITY.get().and_then(|pi| {
        build_peer_server_cfg(pi)
            .inspect_err(|e| log::error!("ERROR: server config builder failed: {e}"))
            .ok()
    });

    let mut endpoint =
        Endpoint::new(EndpointConfig::default(), server_cfg, socket, default_runtime().unwrap())
            .unwrap();

    if let Ok(addr) = endpoint.local_addr() {
        info!("API: ENDPOINT BIND TO {}", addr);
    }

    let root_ca_bytes = jni_try!(read_raw_res(&mut env, &context, "root_ca"));
    let roots = jni_try!(load_root_ca_bytes(&root_ca_bytes));

    let mut client_cfg = jni_try!(build_client_cfg(ProtoRole::Client, &roots));

    let mut transport_cfg = TransportConfig::default();
    transport_cfg.keep_alive_interval(Some(Duration::from_secs(15)));

    client_cfg.transport_config(Arc::new(transport_cfg));

    endpoint.set_default_client_config(client_cfg);

    ENDPOINT.set(Arc::new(endpoint)).expect("init was ran twice");
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn shouldLaunchApp(_: JNIEnv, _: JC) -> bool {
    Identity::public_key().is_ok()
}
