use jni::{JNIEnv, objects::JByteArray};
use jni_macro::jni;
use log::info;

use crate::JC;


#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn parseQRBytes(env: JNIEnv, _: JC, bytes: JByteArray) {
  let bytes = env.convert_byte_array(bytes).unwrap();
  info!("GOT QR CODE : {}", hex::encode(bytes))
}