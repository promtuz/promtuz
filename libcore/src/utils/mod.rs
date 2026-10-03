use std::net::IpAddr;
use std::net::SocketAddr;

use common::node::config::DEFAULT_RELAY_PORT;

pub fn node_short(id: &str) -> String {
    id.get(..8).unwrap_or(id).to_string()
}

pub fn addr_short(addr: SocketAddr) -> String {
    let ip = match addr.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    if addr.port() == DEFAULT_RELAY_PORT {
        ip.to_string()
    } else {
        SocketAddr::new(ip, addr.port()).to_string()
    }
}

pub fn addrs_short(list: &[SocketAddr]) -> String {
    list.iter().map(|a| addr_short(*a)).collect::<Vec<_>>().join(", ")
}

/// A storage error is worth retrying; every other failure of an authenticated payload is final.
pub fn is_storage_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<rusqlite::Error>())
}
