use serde::Deserialize;
use serde::Serialize;

use common::proto::mls_wire::Invite;

/// Payload encoded into a user's identity QR: the long-term IPK, a display
/// name, and a bearer [`Invite`] authorizing the scanner to pair. No live
/// address — pairing is async over the DHT now. Encoded/decoded via the
/// `Packer`/`Unpacker` (postcard) blanket impls.
#[derive(Serialize, Deserialize, Debug)]
pub struct IdentityQr {
    pub ipk: [u8; 32],
    pub name: String,
    pub invite: Invite,
}
