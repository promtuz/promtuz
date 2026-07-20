//! The single P2P connect gate. Contacts-only; `RelayedOnly` is defined for
//! the Privacy editor to write later (v1 returns only `Direct` | `No`).

use crate::data::contact::Contact;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Direct,
    RelayedOnly,
    No,
}

pub fn may_connect(ipk: &[u8; 32]) -> Decision {
    if Contact::is_paired(ipk) { Decision::Direct } else { Decision::No }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stranger_is_denied() {
        assert!(matches!(may_connect(&[0xAB; 32]), Decision::No)); // not a paired contact
    }
}
