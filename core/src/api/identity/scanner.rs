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
use crate::data::idqr::IdentityQr;

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

    if let Some(_ep) = ENDPOINT.get().cloned() {
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

        // ep.connect_with(config, identity.addr, server_name)
    }
}
