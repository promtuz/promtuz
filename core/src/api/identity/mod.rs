// use crate;

use std::net::SocketAddr;

use common::msg::cbor::FromCbor;
use jni::JNIEnv;
use jni::objects::JObject;
use jni::objects::JValue;
use jni_macro::jni;
use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

mod qr;
mod scanner;

use crate::ENDPOINT;
use crate::JC;
use crate::JVM;
use crate::RUNTIME;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::data::relay::Relay;
use crate::events::identity::IdentityEv;
use crate::quic::server::RELAY;
use crate::unwrap_or_ret;

// static ESK: RwLock<Option<StaticSecret>> = RwLock::new(None);
static CONN_CANCEL: Lazy<Mutex<Option<CancellationToken>>> = Lazy::new(|| Mutex::new(None));

async fn get_addr(relay: &Relay) -> Option<SocketAddr> {
    let ep = ENDPOINT.get().unwrap();
    let local = ep.local_addr().ok()?; // no await yet

    let ip = relay.public_addr().await?;
    Some(SocketAddr::new(ip, local.port()))
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityInit(env: JNIEnv, _: JC, class: JObject) {
    info!("IDENTITY: INIT");
    let class = env.new_global_ref(class).unwrap();

    let identity = unwrap_or_ret!(Identity::get());

    RUNTIME.spawn(async move {
        let relay = RELAY.read().clone()?;

        let addr = get_addr(&relay).await?;

        let qr = IdentityQr {
            ipk: identity.ipk(),
            // vfk: identity.vfk(),
            // epk: epk.to_bytes(),
            addr,
            name: identity.name(),
        };

        let jvm = JVM.get().unwrap();
        let mut env = jvm.attach_current_thread().unwrap();
        let qr_arr = &env.byte_array_from_slice(&qr.to_bytes()).unwrap();
        env.call_method(&class, "onQRCreate", "([B)V", &[JValue::Object(qr_arr)]).unwrap();

        let ep = ENDPOINT.get().unwrap().clone();

        let token = CancellationToken::new();
        {
            let mut guard = CONN_CANCEL.lock();
            if let Some(old) = guard.take() {
                old.cancel();
            }
            *guard = Some(token.clone())
        }

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        info!("IDENTITY: conn loop cancelled");
                        break;
                    }
                    incoming = ep.accept() => {
                        let Some(incoming) = incoming else { continue };
                        let Ok(conn) = incoming.await else { continue };
                        let Ok((_tx, mut rx)) = conn.accept_bi().await else { continue };

                        let len = rx.read_u32().await.ok()? as usize;
                        let mut packet = vec![0; len];
                        rx.read_exact(&mut packet).await.ok()?;

                        let msg = IdentityEv::from_cbor(&packet);
                        info!("P2P_MSG: {msg:?}");
                        break;
                    }
                }
            }

            Some(())
        });

        Some(())
    });
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityDestroy(
    _: JNIEnv,
    _class: JC,
    // Further Arguments
) {
    info!("IDENTITY: DESTROY");
    if let Some(tok) = CONN_CANCEL.lock().take() {
        tok.cancel();
    }
}
