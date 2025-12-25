// use crate;

use jni::{JNIEnv, objects::JClass};
use log::info;
use jni_macro::jni;

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityInit(
  _: JNIEnv,
  _class: JClass,
  // Further Arguments
) {
  info!("IDENTITY: INIT")
}