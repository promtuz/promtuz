//! Startup and shutdown shared by the relay, resolver and gateway.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use quinn::Endpoint;
use serde::de::DeserializeOwned;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;

use crate::node::config::NetworkConfig;
use crate::node::enroll::ensure_enrolled;
use crate::node::enroll::spawn_config_reload;
use crate::quic::CloseReason;
use crate::quic::config::build_server_cfg;
use crate::quic::id::NodeId;
use crate::quic::protorole::ProtoRole;
use crate::quic::tunnel_listener::Running;

/// Exits the process when the config cannot be read or parsed.
pub fn load<C: DeserializeOwned>(path: &Path) -> C {
    let read = std::fs::read_to_string(path);
    let raw = crate::graceful!(read, format!("reading config {}", path.display()));
    crate::graceful!(toml::from_str(&raw), "parsing config")
}

/// Starts logging, waits until enrolled and arms config reload. Returns the node key.
pub async fn start<C: DeserializeOwned>(
    name: &str, version: &str, net: &NetworkConfig, config: &Path, level: Option<&str>,
) -> Result<SigningKey> {
    print!("\x1B[2J\x1B[1;1H");
    let _ = std::io::stdout().flush();
    super::log::init(level);
    crate::info!("pz{name} {version}");

    // Waits for a valid cert when unenrolled, so the endpoint is built with usable TLS material.
    let key = ensure_enrolled(net, &net.key_path.with_extension("csr"), name).await?;
    if net.watch_reload {
        spawn_config_reload::<C>(config.to_path_buf());
    }
    let public = key.verifying_key().to_bytes();
    crate::info!("{name} IPK({}) ID({})", hex::encode_upper(public), NodeId::new(public));
    Ok(key)
}

/// Serves `roles` with the CA-issued cert.
pub fn bind(net: &NetworkConfig, roles: &'static [ProtoRole], name: &str) -> Endpoint {
    let server = crate::graceful!(
        build_server_cfg(&net.cert_path, &net.key_path, roles),
        "building the TLS server config"
    );
    let endpoint =
        crate::graceful!(Endpoint::server(server, net.bind_addr()), "starting the QUIC endpoint");
    if let Ok(addr) = endpoint.local_addr() {
        crate::info!("{name} listening at QUIC({addr:?})");
    }
    endpoint
}

/// Ctrl-C, or the SIGTERM systemd sends on stop and restart.
pub async fn shutdown_signal() {
    let mut term = crate::graceful!(signal(SignalKind::terminate()), "listening for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}

/// Closes every connection, gives peers 5 s to acknowledge, then stops the TLS fallback.
pub async fn stop(endpoint: &Endpoint, tunnel: Option<Running>) {
    println!();
    endpoint.close(CloseReason::ShuttingDown.code(), b"ShuttingDown");
    let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.wait_idle()).await;
    if let Some(tunnel) = tunnel {
        tunnel.shutdown().await;
    }
    crate::info!("stopped");
    super::log::flush();
}
