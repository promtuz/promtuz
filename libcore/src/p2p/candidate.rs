//! Host candidates: every interface address a remote peer could reach.

use std::net::IpAddr;
use std::net::SocketAddr;

pub fn local_candidates(port: u16) -> Vec<SocketAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .map(|iface| iface.ip())
        .filter(|ip| !ip.is_loopback() && !is_unroutable(ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Private IPv4 stays routable: two peers on one LAN punch through it.
fn is_unroutable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 192.0.0.0/24 holds the 464XLAT CLAT address of v6-only carriers, never reachable.
            v4.is_link_local() || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        },
        // fe80::/10 link-local and fec0::/10 site-local.
        IpAddr::V6(v6) => {
            let hi = v6.segments()[0] & 0xffc0;
            hi == 0xfe80 || hi == 0xfec0
        },
    }
}
