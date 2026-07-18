//! Candidate addresses — the places a peer might reach us.
//!
//! For now just the local ones: every routable interface IP paired with
//! the P2P socket's port. Loopback and link-local are dropped (a remote
//! peer can't use them). A global IPv6 address here often needs no punch
//! at all — Jio/Airtel hand out un-NATed v6, so the local address *is*
//! the reachable one. The server-reflexive (post-NAT v4) candidate comes
//! later, from the relay's STUN echo.

use std::net::IpAddr;
use std::net::SocketAddr;

/// Our local candidate addresses, each paired with `port` (the P2P
/// socket's bound port). Empty if the interface list can't be read.
pub fn local_candidates(port: u16) -> Vec<SocketAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .map(|iface| iface.ip())
        .filter(|ip| !ip.is_loopback() && !is_link_local(ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Link-local addresses (IPv4 169.254/16, IPv6 fe80::/10) are only
/// reachable on the same physical link, never from a remote peer.
fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // `Ipv6Addr::is_unicast_link_local` is still unstable, so match
        // the fe80::/10 prefix directly.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn classifies_link_local() {
        assert!(is_link_local(&"169.254.1.1".parse::<Ipv4Addr>().unwrap().into()));
        assert!(is_link_local(&"fe80::1".parse::<Ipv6Addr>().unwrap().into()));
        // routable addresses are not link-local
        assert!(!is_link_local(&"192.168.1.5".parse::<Ipv4Addr>().unwrap().into()));
        assert!(!is_link_local(&"2401:4900::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn gather_pairs_port_and_drops_loopback() {
        let cands = local_candidates(4242);
        for addr in &cands {
            assert_eq!(addr.port(), 4242);
            assert!(!addr.ip().is_loopback(), "loopback leaked: {addr}");
            assert!(!is_link_local(&addr.ip()), "link-local leaked: {addr}");
        }
    }
}
