use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::node::config::DEFAULT_RELAY_PORT;

#[macro_use]
pub mod macros;

/// Short display form of a relay/peer node id: the leading 8 chars of the
/// base32 public key. No canonical short id exists, and 8 chars is enough to
/// eyeball-match a node across log lines without the full 52-char key.
pub fn node_short(id: &str) -> String {
    id.get(..8).unwrap_or(id).to_string()
}

/// Compact address for logs: unwrap IPv4-mapped IPv6 (`[::ffff:1.2.3.4]:p` →
/// `1.2.3.4:p`) and drop the port when it's the default relay/QUIC port (the
/// common case, so `ip` alone is unambiguous).
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

/// Comma-join a candidate list through [`addr_short`], for the P2P offer dumps.
pub fn addrs_short(list: &[SocketAddr]) -> String {
    list.iter().map(|a| addr_short(*a)).collect::<Vec<_>>().join(", ")
}

/// ### TEMPORARY:
/// uses google's dns to verify internet availability
pub fn has_internet() -> bool {
    TcpStream::connect_timeout(&"8.8.8.8:53".parse().unwrap(), Duration::from_secs(2)).is_ok()
}

pub fn systime() -> Duration {
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0))
}
