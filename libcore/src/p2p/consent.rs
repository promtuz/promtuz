//! Paired contacts may connect directly. Active group co-members may transfer
//! through a relay without gaining access to our public or LAN addresses.

use crate::data::contact::Contact;
use crate::db::Stores;
use crate::state::core;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Direct,
    RelayedOnly,
    No,
}

pub fn may_connect(ipk: &[u8; 32]) -> Decision {
    may_connect_in(&core().db, ipk)
}

/// The group branch still reads the process's identity and messages.
pub(crate) fn may_connect_in(db: &Stores, ipk: &[u8; 32]) -> Decision {
    if Contact::is_paired_tx(&db.contacts().lock(), ipk) {
        Decision::Direct
    } else if crate::data::conversation::Conversation::for_peer_transport(ipk, false).is_some() {
        Decision::RelayedOnly
    } else {
        Decision::No
    }
}
