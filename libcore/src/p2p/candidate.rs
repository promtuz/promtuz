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
        .filter(|ip| !ip.is_loopback() && !is_unroutable(ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Addresses a remote peer can never reach: IPv4 link-local (169.254/16),
/// IPv6 link-local (fe80::/10), and deprecated IPv6 site-local (fec0::/10,
/// seen mainly on emulators). Private IPv4 (192.168/10/172.16) is kept —
/// two peers on one LAN punch through it.
fn is_unroutable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // `Ipv6Addr::is_unicast_link_local` is unstable, so match prefixes
        // directly: fe80::/10 link-local, fec0::/10 site-local.
        IpAddr::V6(v6) => {
            let hi = v6.segments()[0] & 0xffc0;
            hi == 0xfe80 || hi == 0xfec0
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn classifies_unroutable() {
        assert!(is_unroutable(&"169.254.1.1".parse::<Ipv4Addr>().unwrap().into()));
        assert!(is_unroutable(&"fe80::1".parse::<Ipv6Addr>().unwrap().into()));
        // deprecated site-local (what the emulator advertised)
        assert!(is_unroutable(&"fec0::5054:ff:fe12:3456".parse::<Ipv6Addr>().unwrap().into()));
        // routable addresses (private LAN v4 + global v6) are kept
        assert!(!is_unroutable(&"192.168.1.5".parse::<Ipv4Addr>().unwrap().into()));
        assert!(!is_unroutable(&"2409:4117::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn gather_pairs_port_and_drops_loopback() {
        let cands = local_candidates(4242);
        for addr in &cands {
            assert_eq!(addr.port(), 4242);
            assert!(!addr.ip().is_loopback(), "loopback leaked: {addr}");
            assert!(!is_unroutable(&addr.ip()), "unroutable leaked: {addr}");
        }
    }
}
