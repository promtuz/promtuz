
use anyhow::anyhow;
use common::crypto::get_static_keypair;
use common::proto::Sender;
use common::proto::client_peer::ClientPeerPacket;
use common::proto::client_peer::IdentityP;
use common::proto::pack::Unpacker;
use common::quic::id::UserId;
use jni::JNIEnv;
use jni::objects::JByteArray;
use jni::objects::JValue;
use jni_macro::jni;
use log::debug;
use log::error;
use log::info;

use crate::ENDPOINT;
use crate::JC;
use crate::KEY_MANAGER;
use crate::RUNTIME;
use crate::api::PEER_IDENTITY;
use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::identity::IdentitySigner;
use crate::data::idqr::IdentityQr;
use crate::quic::peer_config::build_peer_client_cfg;
use crate::quic::peer_config::extract_peer_tls_pubkey;
use crate::quic::peer_config::ipk_binding_message;
use crate::quic::peer_config::verify_ipk_binding;
use crate::try_ret;

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn parseQRBytes(mut env: JNIEnv, _: JC, bytes: JByteArray) {
    let bytes = env.convert_byte_array(bytes).unwrap();

    // this is peer's identity qr
    let peer_identity_qr = try_ret!(
        IdentityQr::decode(&bytes).map_err(|e| error!("ERROR: failed to parse qr code: {e}"))
    );

    debug!("DEBUG: parsed qr code: {peer_identity_qr:?}");

    let peer_ipk_hex = hex::encode(peer_identity_qr.ipk);

    if let Some(ep) = ENDPOINT.get().cloned() {
        match env.call_static_method(
            "com/promtuz/chat/presentation/viewmodel/QrScannerVM",
            "onIdentityQrScanned",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&env.new_string(&peer_identity_qr.name).unwrap())],
        ) {
            Ok(_) => {
                info!("Successfully triggered identity processing for: {}", peer_identity_qr.name)
            },
            Err(e) => error!("Failed to call onIdentityQrScanned: {:?}", e),
        }

        // our own peer identity used to connect with peers, not peer's identity
        let our_peer_identity = PEER_IDENTITY.get().unwrap();

        RUNTIME.spawn(async move {
            let block = async move {
                let our_name =
                    Identity::get().map(|i| i.name()).ok_or(anyhow!("could not find ipk"))?;

                let conn = ep
                    .connect_with(
                        build_peer_client_cfg(our_peer_identity)?,
                        peer_identity_qr.addr,
                        &UserId::derive(&peer_identity_qr.ipk).to_string(),
                    )?
                    .await?;

                debug!(
                    "DEBUG: connected with peer({}) on {}",
                    &peer_ipk_hex, peer_identity_qr.addr
                );

                let (mut send, mut recv) = conn.open_bi().await?;

                // Generate a unique ephemeral keypair for this friendship
                let (our_esk, our_epk) = get_static_keypair();

                {
                    use IdentityP::*;

                    // Build the IPK<->TLS-subkey binding for our outgoing
                    // AddMe so the sharer can verify our long-term IPK
                    // matches the cert SPKI it just handshaked against.
                    let our_tls_subkey = IdentitySigner::tls_subkey()?;
                    let binding_msg =
                        ipk_binding_message(&our_tls_subkey.verifying_key().to_bytes());
                    let (our_ipk_sig, our_ipk) = IdentitySigner::sign_with_ipk(&binding_msg)?;

                    ClientPeerPacket::Identity(AddMe {
                        epk: our_epk.to_bytes(),
                        name: our_name.clone(),
                        ipk: our_ipk,
                        ipk_sig: our_ipk_sig.to_bytes(),
                    })
                    .send(&mut send)
                    .await?;

                    use ClientPeerPacket::*;

                    while let Ok(Identity(packet)) = ClientPeerPacket::unpack(&mut recv).await {
                        match packet {
                            AddedYou { epk, ipk: claimed_ipk, ipk_sig } => {
                                // Verify the sharer's IPK signs the TLS sub-key
                                // we just handshaked against. The QR-advertised
                                // IPK is the source of truth for *which* peer
                                // we expect; if either the binding fails or the
                                // claimed IPK doesn't match the QR, abort and
                                // do not save.
                                let peer_tls_pubkey = extract_peer_tls_pubkey(&conn).ok_or_else(
                                    || anyhow!("failed to extract peer TLS pubkey from cert"),
                                )?;
                                if claimed_ipk != peer_identity_qr.ipk {
                                    error!(
                                        "ERROR: sharer's claimed IPK does not match scanned QR"
                                    );
                                    break;
                                }
                                if let Err(e) = verify_ipk_binding(
                                    &claimed_ipk,
                                    &peer_tls_pubkey,
                                    &ipk_sig,
                                ) {
                                    error!("ERROR: peer IPK<->TLS binding rejected: {e}");
                                    break;
                                }

                                info!(
                                    "INFO: *{}* has accepted the request with EPK({})",
                                    &peer_identity_qr.name,
                                    hex::encode(epk)
                                );

                                // Encrypt our ephemeral secret for storage
                                let enc_esk = {
                                    let km = KEY_MANAGER.get().unwrap();
                                    km.encrypt(&our_esk.to_bytes()).expect("failed to encrypt esk")
                                };

                                match Contact::save(
                                    peer_identity_qr.ipk,
                                    epk,
                                    enc_esk,
                                    peer_identity_qr.name.clone(),
                                ) {
                                    Ok(_) => {
                                        info!("INFO: saved contact {}", peer_identity_qr.name);

                                        // Confirm to sharer so they can save too
                                        ClientPeerPacket::Identity(Confirmed)
                                            .send(&mut send)
                                            .await?;
                                        send.finish()?;
                                    },
                                    Err(e) => error!("ERROR: failed to save contact: {e}"),
                                }
                            },
                            No { reason } => {
                                info!(
                                    "INFO: *{}* rejected request: {reason}",
                                    &peer_identity_qr.name
                                )
                            },
                            _ => { /* shouldn't reach this */ },
                        }
                    }
                }

                Ok::<(), anyhow::Error>(())
            }
            .await;

            if let Some(err) = block.err() {
                error!(
                    "ERROR: connection with peer({}) failed: {err}",
                    hex::encode(peer_identity_qr.ipk)
                )
            }
        });
    }
}
