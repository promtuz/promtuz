//! Paired contacts may connect directly. Active group co-members may transfer
//! through a relay without gaining access to our public or LAN addresses.

use crate::data::contact::Contact;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Direct,
    RelayedOnly,
    No,
}

pub fn may_connect(ipk: &[u8; 32]) -> Decision {
    if Contact::is_paired(ipk) {
        Decision::Direct
    } else if crate::data::conversation::Conversation::for_peer_transport(ipk, false).is_some() {
        Decision::RelayedOnly
    } else {
        Decision::No
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stranger_is_denied() {
        assert!(matches!(may_connect(&[0xAB; 32]), Decision::No)); // not a paired contact
    }
}
