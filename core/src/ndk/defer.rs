use std::sync::OnceLock;

use jni::JNIEnv;
use jni::objects::GlobalRef;
use jni::objects::JObject;
use jni::objects::JValue;
use jni::sys::jobject;

use crate::JVM;

struct DeferredJni {
    complete: jni::objects::JMethodID,
    complete_ex: jni::objects::JMethodID,
}

static DEFERRED_JNI: OnceLock<DeferredJni> = OnceLock::new();

fn deferred_jni(env: &mut JNIEnv) -> &'static DeferredJni {
    DEFERRED_JNI.get_or_init(|| {
        let cls = env.find_class("kotlinx/coroutines/CompletableDeferred").unwrap();

        DeferredJni {
            complete: env.get_method_id(&cls, "complete", "(Ljava/lang/Object;)Z").unwrap(),
            complete_ex: env
                .get_method_id(&cls, "completeExceptionally", "(Ljava/lang/Throwable;)Z")
                .unwrap(),
        }
    })
}

pub struct KotlinDeferred {
    inner: GlobalRef,
}

/// SECURITY:
/// Uses `call_method_unchecked` with cached `JMethodID`s.
/// Correctness relies on exact signature match and stable target classes.
/// Any mismatch is undefined behavior (JVM crash / memory corruption).
impl KotlinDeferred {
    pub fn cache(env: &mut JNIEnv) {
        deferred_jni(env);
    }

    pub fn new(env: &mut JNIEnv) -> (Self, jobject) {
        let obj = env
            .call_static_method(
                "kotlinx/coroutines/CompletableDeferredKt",
                "CompletableDeferred",
                "(Lkotlinx/coroutines/Job;)Lkotlinx/coroutines/CompletableDeferred;",
                &[JValue::Object(&JObject::null())],
            )
            .unwrap()
            .l()
            .unwrap();

        let global = env.new_global_ref(&obj).unwrap();
        (Self { inner: global }, obj.into_raw())
    }

    pub fn complete_object(self, obj: &JObject) {
        let jvm = JVM.get().unwrap();
        let mut env = jvm.attach_current_thread().unwrap();
        let jni = deferred_jni(&mut env);

        unsafe {
            env.call_method_unchecked(
                self.inner.as_obj(),
                jni.complete,
                jni::signature::ReturnType::Primitive(jni::signature::Primitive::Boolean),
                &[JValue::Object(obj).as_jni()],
            )
        }
        .unwrap();
    }

    pub fn fail_str(self, msg: &str) {
        let jvm = JVM.get().unwrap();
        let mut env = jvm.attach_current_thread().unwrap();
        let jni = deferred_jni(&mut env);

        let strarg = &JObject::from(env.new_string(msg).unwrap());
        let strarg = strarg.into();

        let ex = env
            .new_object("java/lang/RuntimeException", "(Ljava/lang/String;)V", &[strarg])
            .unwrap();

        unsafe {
            env.call_method_unchecked(
                self.inner.as_obj(),
                jni.complete_ex,
                jni::signature::ReturnType::Primitive(jni::signature::Primitive::Boolean),
                &[JValue::Object(&ex).as_jni()],
            )
        }
        .unwrap();
    }
}
