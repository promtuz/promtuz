use anyhow::anyhow;
use common::crypto::get_static_keypair;
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
use crate::RUNTIME;
use crate::api::PEER_IDENTITY;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::quic::peer_config::build_peer_client_cfg;
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
                let (our_ipk, our_name) = Identity::get()
                    .map(|i| (i.ipk(), i.name()))
                    .ok_or(anyhow!("could not find ipk"))?;

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

                {
                    use IdentityP::*;
                    let (_esk, epk) = get_static_keypair();

                    ClientPeerPacket::Identity(AddMe {
                        epk: epk.to_bytes(),
                        name: our_name.clone(),
                    })
                    .send(&mut send)
                    .await?;

                    use ClientPeerPacket::*;

                    while let Ok(Identity(packet)) = ClientPeerPacket::unpack(&mut recv).await {
                        match packet {
                            AddedYou { epk } => {
                                // epk of sharer
                                info!(
                                    "INFO: *{}* has accepted the request with EPK({})",
                                    &peer_identity_qr.name,
                                    hex::encode(epk)
                                )
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
