//! Identity exports: enrollment + QR invite pairing.

use common::proto::mls_wire::PairingP;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::utils::now_ms;

use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::messaging;
use crate::platform::CoreError;
use crate::state::core;

#[uniffi::export]
pub fn enroll(name: String) -> Result<(), CoreError> {
    Identity::create(&name)?;
    Ok(())
}

/// True once our KeyPackage is on a quorum of homes, so a QR shared now can be paired with.
#[uniffi::export]
pub fn kp_publish_ready() -> bool {
    crate::mls::scheduler::kp_publish_ready()
}

/// Whoever scans it may add us until the invite expires after 10 minutes. Several scanners can use
/// one QR, and minting needs no relay.
#[uniffi::export]
pub fn make_invite_qr() -> Result<Vec<u8>, CoreError> {
    let identity =
        Identity::get().ok_or_else(|| CoreError::Internal { msg: "no identity".into() })?;
    let invite = Identity::mint_invite()?;
    let qr = IdentityQr { ipk: identity.ipk(), name: identity.name(), invite };
    qr.ser().map_err(|e| CoreError::Internal { msg: format!("qr encode: {e}") })
}

/// Pairs in the background; the `Result` only reports a malformed QR or a missing identity.
#[uniffi::export]
pub fn pair_from_qr(qr_bytes: Vec<u8>) -> Result<(), CoreError> {
    let qr = IdentityQr::deser(&qr_bytes)
        .map_err(|e| CoreError::Internal { msg: format!("bad qr: {e}") })?;
    let me = Identity::get().ok_or_else(|| CoreError::Internal { msg: "no identity".into() })?;
    if qr.ipk == me.ipk() {
        return Err(CoreError::Internal { msg: "cannot pair with yourself".into() });
    }
    let pairing = PairingP { invite: qr.invite, sender_name: me.name() };

    // `pair` saves the contact only once the Welcome is out, so an unreachable peer leaves no row.
    let (to, peer_name) = (qr.ipk, qr.name);
    core().spawn(async move {
        if let Err(e) = messaging::welcome::pair(to, peer_name, pairing).await {
            log::error!("PAIR: {e}");
        }
    });
    Ok(())
}

#[derive(uniffi::Record)]
pub struct InvitePreview {
    pub ipk: Vec<u8>,
    pub name: String,
    pub already_contact: bool,
    pub expired: bool,
    /// Unix ms, for a live countdown.
    pub expiry_ms: u64,
}

/// Decodes an invite for the confirmation sheet without pairing.
#[uniffi::export]
pub fn preview_invite(qr_bytes: Vec<u8>) -> Result<InvitePreview, CoreError> {
    let qr = IdentityQr::deser(&qr_bytes)
        .map_err(|e| CoreError::Internal { msg: format!("bad invite: {e}") })?;
    let now_ms = now_ms();
    Ok(InvitePreview {
        ipk: qr.ipk.to_vec(),
        name: qr.name.chars().take(32).collect(),
        already_contact: Contact::exists(&qr.ipk),
        expired: qr.invite.expiry_ms < now_ms,
        expiry_ms: qr.invite.expiry_ms,
    })
}
