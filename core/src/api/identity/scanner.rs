use jni::JNIEnv;
use jni::objects::JByteArray;
use jni_macro::jni;
use log::debug;
use log::error;
use log::info;

use crate::JC;
use crate::data::idqr::IdentityQr;

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn parseQRBytes(env: JNIEnv, _: JC, bytes: JByteArray) {
    let bytes = env.convert_byte_array(bytes).unwrap();
    info!("GOT QR CODE : {}", hex::encode(&bytes));

    let identity = match IdentityQr::decode(&bytes) {
        Ok(iqr) => iqr,
        Err(err) => {
            error!("PARSE_QR: {err}");

            return;
        },
    };

    // FREEZE THE SCANNER

    //

    debug!("IDENTITY_QR: {identity:?}");
}
