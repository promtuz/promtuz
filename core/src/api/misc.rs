use jni::JNIEnv;
use jni::objects::JObject;
use jni::sys::jobject;
use jni_macro::jni;

use crate::JC;
use crate::RUNTIME;
use crate::ndk::defer::KotlinDeferred;
use crate::quic::server::RELAY;
use crate::utils::AsJni;

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getPublicAddr(mut env: JNIEnv, _: JC) -> jobject {
    let (deferred, raw) = KotlinDeferred::new(&mut env);

    KotlinDeferred::cache(&mut env);

    RUNTIME.spawn(async move {
        let relay = RELAY.read().clone();

        let res = match relay {
            Some(r) => r.public_addr().await,
            None => None,
        };

        match res {
            Some(addr) => deferred.complete_object(addr.as_jni().as_obj()),
            None => deferred.complete_object(&JObject::null()),
        }
    });

    raw
}
