#include <jni.h>
#include<cstdio>
#include<cstring>

#include "include/core_bindings.h"
//
//
//extern "C" {
//int c_get_static_key(uint8_t *sk_ptr) {
//    uint8_t sk[] = {1, 2, 3, 3, 3, 1, 2, 2, 3, 2, 4, 235, 2, 12, 3, 2, 2, 50, 0, 0, 0, 1, 2, 3, 241,
//                    221, 59, 21, 32, 43, 54, 3};
//
//    memcpy(sk_ptr, sk, sizeof(sk));
//
//    return 0;
//}
//}


extern "C"
JNIEXPORT jbyteArray JNICALL
Java_com_promtuz_chat_native_Core_getStaticKey(JNIEnv *env, jobject thiz) {
    jbyteArray arr = env->NewByteArray(32);
    jbyte *buf = env->GetByteArrayElements(arr, nullptr);
    c_get_static_key(reinterpret_cast<uint8_t *>(buf));
    env->ReleaseByteArrayElements(arr, buf, 0);

    return arr;
}
