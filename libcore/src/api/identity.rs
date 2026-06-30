//! Identity exports: enrollment + QR invite pairing.

use common::proto::mls_wire::PairingP;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;

use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::messaging;
use crate::platform::CoreError;

/// Enroll — create the long-term identity. The client calls this from the
/// enrollment screen (shown when `should_launch_app()` is false).
#[uniffi::export]
pub fn enroll(name: String) -> Result<(), CoreError> {
    Identity::create(&name)?;
    Ok(())
}

/// Mint a fresh pairing invite and return the QR payload bytes to render.
/// Whoever scans it may add us until the invite expires (~10 min). Needs
/// no relay connection — built from our identity alone. The same QR works
/// for multiple scanners within the window (no per-scan refresh).
#[uniffi::export]
pub fn make_invite_qr() -> Result<Vec<u8>, CoreError> {
    let identity =
        Identity::get().ok_or_else(|| CoreError::Internal { msg: "no identity".into() })?;
    let invite = Identity::mint_invite()?;
    let qr = IdentityQr { ipk: identity.ipk(), name: identity.name(), invite };
    qr.ser().map_err(|e| CoreError::Internal { msg: format!("qr encode: {e}") })
}

/// Pair from a scanned identity QR: save the sharer as a contact, then
/// (eagerly, in the background) create the 1:1 MLS group and publish a
/// Welcome carrying their invite + our name, so their device accepts us.
/// The synchronous `Result` only reports a malformed QR or missing
/// identity; the pairing outcome surfaces via the contact list / events.
#[uniffi::export]
pub fn pair_from_qr(qr_bytes: Vec<u8>) -> Result<(), CoreError> {
    let qr = IdentityQr::deser(&qr_bytes)
        .map_err(|e| CoreError::Internal { msg: format!("bad qr: {e}") })?;
    let our_name = Identity::get()
        .ok_or_else(|| CoreError::Internal { msg: "no identity".into() })?
        .name();

    // We have the sharer's identity from the QR — save them right away.
    let _ = Contact::save(qr.ipk, qr.name);

    let to = qr.ipk;
    let pairing = PairingP { invite: qr.invite, sender_name: our_name };
    crate::RUNTIME.spawn(async move {
        if let Err(e) = messaging::pair(to, pairing).await {
            log::error!("PAIR: {e}");
        }
    });
    Ok(())
}
