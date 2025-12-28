use jni::AttachGuard;
use jni::JNIEnv;
use jni::objects::GlobalRef;
use jni::objects::JByteArray;
use jni::objects::JClass;
use jni::objects::JObject;
use jni::objects::JStaticMethodID;
use jni::objects::JValue;

use crate::JVM;

pub struct KeyManager {
    class: GlobalRef,
    enc_mid: JStaticMethodID,
    dec_mid: JStaticMethodID,
}

const KEY_MANAGER_CLASS: &str = "com/promtuz/chat/security/KeyManager";

impl KeyManager {
    pub fn new(env: &mut JNIEnv) -> jni::errors::Result<Self> {
        let local_class = env.find_class(KEY_MANAGER_CLASS)?;
        let class = env.new_global_ref(&local_class)?;

        let enc_mid = env.get_static_method_id(&local_class, "encrypt", "([B)[B")?;
        let dec_mid = env.get_static_method_id(&local_class, "decrypt", "([B)[B")?;

        Ok(Self { enc_mid, dec_mid, class })
    }

    fn env(&'_ self) -> AttachGuard<'_> {
        JVM.get().unwrap().attach_current_thread().unwrap()
    }

    pub fn encrypt(&self, data: &[u8]) -> jni::errors::Result<Vec<u8>> {
        let mut env = self.env();

        let input = env.byte_array_from_slice(data)?;
        let out = env
            .call_static_method(
                <&JClass>::from(self.class.as_obj()),
                "encrypt",
                "([B)[B",
                &[JValue::Object(&input)],
            )?
            .l()?;

        Self::byte_array_to_vec(&mut env, out)
    }

    pub fn decrypt(&self, data: &[u8]) -> jni::errors::Result<Vec<u8>> {
        let mut env = self.env();

        let input = env.byte_array_from_slice(data)?;
        let out = env
            .call_static_method(
                <&JClass>::from(self.class.as_obj()),
                "decrypt",
                "([B)[B",
                &[JValue::Object(&input)],
            )?
            .l()?;

        Self::byte_array_to_vec(&mut env, out)
    }

    fn byte_array_to_vec(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<Vec<u8>> {
        let arr = JByteArray::from(obj);
        env.convert_byte_array(arr)
    }
}
