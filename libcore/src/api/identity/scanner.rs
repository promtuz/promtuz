use anyhow::anyhow;
use common::crypto::get_static_keypair;
use common::proto::client_peer::ClientPeerPacket;
use common::proto::client_peer::IdentityP;
use common::proto::pack::Unpacker;
use common::quic::id::UserId;
use common::quic::id::derive_user_id;
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

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn parseQRBytes(mut env: JNIEnv, _: JC, bytes: JByteArray) {
    let bytes = env.convert_byte_array(bytes).unwrap();
    info!("GOT QR CODE : {}", hex::encode(&bytes));

    let identity = match IdentityQr::decode(&bytes) {
        Ok(iqr) => iqr,
        Err(err) => {
            error!("PARSE_QR: {err}");

            return;
        },
    };

    debug!("IDENTITY_QR: {identity:?}");
    debug!("PUBLIC ID ?? {}", derive_user_id(&identity.ipk));

    if let Some(ep) = ENDPOINT.get().cloned() {
        // FREEZE THE SCANNER - Call back to Android ViewModel
        match env.call_static_method(
            "com/promtuz/chat/presentation/viewmodel/QrScannerVM",
            "onIdentityQrScanned",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&env.new_string(&identity.name).unwrap())],
        ) {
            Ok(_) => info!("Successfully triggered identity processing for: {}", identity.name),
            Err(e) => error!("Failed to call onIdentityQrScanned: {:?}", e),
        }

        let peer_identity = PEER_IDENTITY.get().unwrap();

        RUNTIME.spawn(async move {
            let block = async move {
                debug!("IN ASYNC RUNTIME");
                let (ipk, name) = Identity::get()
                    .map(|i| (i.ipk(), i.name()))
                    .ok_or(anyhow!("could not find ipk"))?;
                debug!("IPK: {ipk:?} AND NAME {name}");

                let conn = ep
                    .connect_with(
                        build_peer_client_cfg(peer_identity)?,
                        identity.addr,
                        &UserId::derive(&identity.ipk).to_string(),
                    )?
                    .await?;

                debug!("CONNECTED");

                let (mut send, mut recv) = conn.open_bi().await?;

                debug!("OPENED BI STREAM");

                use IdentityP::*;
                let (_esk, epk) = get_static_keypair();

                ClientPeerPacket::Identity(AddMe { ipk, epk: epk.to_bytes(), name })
                    .send(&mut send)
                    .await?;

                while let Ok(packet) = ClientPeerPacket::unpack(&mut recv).await {
                    debug!("PEER_PACKET: {packet:?}");
                }

                Ok::<(), anyhow::Error>(())
            }
            .await;

            if let Some(err) = block.err() {
                log::error!("SCANNER_ERR: {err}")
            }
        });
    }
}
