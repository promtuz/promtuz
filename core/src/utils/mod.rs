use std::net::IpAddr;
use std::net::TcpStream;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::crypto::PublicKey;
use common::crypto::StaticSecret;
use common::crypto::sign::SigningKey;
use jni::objects::GlobalRef;
use jni::objects::JByteArray;
use jni::objects::JObject;

use crate::JVM;

pub mod r#async;
pub mod error;
pub mod ujni;

/// ### TEMPORARY:
/// uses google's dns to verify internet availability
pub fn has_internet() -> bool {
    TcpStream::connect_timeout(&"8.8.8.8:53".parse().unwrap(), Duration::from_secs(2)).is_ok()
}

pub fn systime() -> Duration {
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0))
}

pub trait KeyConversion {
    fn to_bytes(self) -> [u8; 32];
    fn to_public(self) -> PublicKey;
    fn to_secret(self) -> StaticSecret;
    fn to_signing(self) -> SigningKey;
}

impl KeyConversion for JByteArray<'_> {
    fn to_bytes(self) -> [u8; 32] {
        let vm = JVM.get().unwrap();
        let env = vm.attach_current_thread().unwrap();

        let vec_arr = env.convert_byte_array(self).unwrap();
        (*vec_arr).try_into().unwrap()
    }

    fn to_public(self) -> PublicKey {
        PublicKey::from(self.to_bytes())
    }

    fn to_secret(self) -> StaticSecret {
        StaticSecret::from(self.to_bytes())
    }

    fn to_signing(self) -> SigningKey {
        SigningKey::from(self.to_bytes())
    }
}

pub trait AsJni {
    fn as_jni(&'_ self) -> GlobalRef;
}

impl AsJni for IpAddr {
    fn as_jni(&'_ self) -> GlobalRef {
        let vm = JVM.get().unwrap();
        let mut env = vm.attach_current_thread().unwrap();

        let arr = &env.byte_array_from_slice(self.as_octets()).unwrap();

        let obj: JObject = env
            .call_static_method(
                "java/net/InetAddress",
                "getByAddress",
                "([B)Ljava/net/InetAddress;",
                &[arr.into()],
            )
            .unwrap()
            .try_into()
            .unwrap();

        env.new_global_ref(obj).unwrap()
    }
}
