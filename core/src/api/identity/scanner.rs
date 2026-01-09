use anyhow::Result;
use common::quic::config::build_client_cfg;
use common::quic::id::UserId;
use common::quic::id::derive_user_id;
use jni::JNIEnv;
use jni::objects::JByteArray;
use jni::objects::JValue;
use jni_macro::jni;
use log::debug;
use log::error;
use log::info;
use tokio::task::JoinHandle;

use crate::ENDPOINT;
use crate::JC;
use crate::RUNTIME;
use crate::data::idqr::IdentityQr;
use crate::events::identity::IdentityEv;
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

        RUNTIME.spawn(async move {
            let conn = ep
                .connect_with(
                    build_peer_client_cfg().unwrap(),
                    identity.addr,
                    &UserId::derive(&identity.ipk),
                )?
                .await?;

            let (mut send, mut recv) = conn.open_bi().await?;


            // IdentityEv

            Ok::<(), anyhow::Error>(())
        });
    }
}
