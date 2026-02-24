use jni::AttachGuard;
use jni::JNIEnv;
use jni::objects::GlobalRef;
use jni::objects::JByteArray;
use jni::objects::JClass;
use jni::objects::JObject;
use jni::objects::JValue;

use crate::JVM;

#[derive(Debug)]
pub struct KeyManager {
    class: GlobalRef,
}

const KEY_MANAGER_CLASS: &str = "com/promtuz/chat/security/KeyManager";

impl KeyManager {
    /// `env` must be original env passed from JNI
    ///
    /// not from `JVM::attach_current_thread`,
    /// as [`KEY_MANAGER_CLASS`] only exists in original thread
    pub fn new(env: &mut JNIEnv) -> jni::errors::Result<Self> {
        let local_class = env.find_class(KEY_MANAGER_CLASS)?;
        let class = env.new_global_ref(&local_class)?;

        Ok(Self { class })
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
