use serde::Deserialize;
use serde::Serialize;

use common::proto::mls_wire::Invite;

/// Identity QR payload, postcard-encoded. The bearer [`Invite`] authorizes the scanner to pair.
#[derive(Serialize, Deserialize, Debug)]
pub struct IdentityQr {
    pub ipk: [u8; 32],
    pub name: String,
    pub invite: Invite,
}
