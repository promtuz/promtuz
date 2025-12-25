//! Utilities for JNI

use jni::JNIEnv;
use jni::objects::JByteArray;
use jni::objects::JObject;
use jni::objects::JString;
use jni::sys::jint;

pub fn get_package_name(env: &mut JNIEnv, context: &JObject) -> anyhow::Result<String> {
    let pkg_obj = env.call_method(context, "getPackageName", "()Ljava/lang/String;", &[])?.l()?;
    Ok(env.get_string(&JString::from(pkg_obj))?.into())
}

/// For eg.
///
/// 1. `get_raw_res_id(env, context, "resolver_seeds")` => R.raw.resolver_seeds
///
/// 2. `get_raw_res_id(env, context, "root_ca")` => R.raw.root_ca
pub fn get_raw_res_id(env: &mut JNIEnv, context: &JObject, name: &str) -> anyhow::Result<jint> {
    let pkg = get_package_name(env, context)?;

    let class = env.find_class(format!("{}/R$raw", pkg.replace('.', "/")))?;

    let value = env.get_static_field(class, name, "I")?;
    Ok(value.i()?)
}

pub fn read_raw_res(env: &mut JNIEnv, context: &JObject, name: &str) -> anyhow::Result<Vec<u8>> {
    let res_id = get_raw_res_id(env, context, name)?;

    // Resources res = context.getResources();
    let resources =
        env.call_method(context, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;

    // InputStream is = res.openRawResource(res_id);
    let input = env
        .call_method(resources, "openRawResource", "(I)Ljava/io/InputStream;", &[res_id.into()])?
        .l()?;

    // byte[] bytes = is.readAllBytes();  (API 26+)
    let bytes = env.call_method(input, "readAllBytes", "()[B", &[])?.l()?;
    let bytes = JByteArray::from(bytes);

    Ok(env.convert_byte_array(bytes)?)
}
