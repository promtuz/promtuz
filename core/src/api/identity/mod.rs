// use std::net::SocketAddr;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_peer::ClientPeerPacket;
use common::proto::client_peer::IdentityP;
use common::proto::pack::Unpacker;
use jni::JNIEnv;
use jni::objects::JObject;
use jni::objects::JValue;
use jni_macro::jni;
use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

mod qr;
mod scanner;

use crate::ENDPOINT;
use crate::JC;
use crate::JVM;
use crate::RUNTIME;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::quic::server::RELAY;

// static ESK: RwLock<Option<StaticSecret>> = RwLock::new(None);
static CONN_CANCEL: Lazy<Mutex<Option<CancellationToken>>> = Lazy::new(|| Mutex::new(None));


#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityInit(env: JNIEnv, _: JC, class: JObject) {
    let class = env.new_global_ref(class).unwrap();

    let identity = Identity::get().expect("identity not found");

    RUNTIME.spawn(async move {
        let block: Result<()> = async move {
            let relay = RELAY.read().clone().ok_or(anyhow!("relay unavailable"))?;

            // we advertise our ACTUAL public address including port as NAT can port forward
            let addr = relay.public_addr().await?;
            info!("IDENTITY: PUBLIC ADDR {addr}");
            
            // if let Ok(addrs) = if_addrs::get_if_addrs() {
            //     for interface in addrs {
            //         info!("INTERFACE: {interface:?}");
            //     }
            // }

            let qr = IdentityQr {
                ipk: identity.ipk(),
                // vfk: identity.vfk(),
                // epk: epk.to_bytes(),
                addr,
                name: identity.name(),
            };

            info!("IDENTITY: QR {qr:?}");

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
                let block: anyhow::Result<()> = async move {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => {
                                info!("IDENTITY: conn loop cancelled");
                                break;
                            }
                            incoming = ep.accept() => {
                                let Some(incoming) = incoming else { continue };
                                let conn = incoming.await.map_err(|e| anyhow!("failed to accept incoming conn: {e}"))?;
                                let (_tx, mut recv) = conn.accept_bi().await.map_err(|e| anyhow!("failed to accept stream: {e}"))?;

                                use ClientPeerPacket::*;
                                use IdentityP::*;
                                loop {
                                    match ClientPeerPacket::unpack(&mut recv).await.map_err(|e| anyhow!("failed to unpack: {e}"))? {
                                        Identity(AddMe { ipk, epk, name }) => {
                                            info!("{name} says add me.\nIPK({})\nEPK({})", hex::encode(ipk), hex::encode(epk))
                                        },
                                    }
                                }
                            }
                        }
                    }

                    Ok(())
                }.await;
                
                if let Some(err) = block.err() {
                    log::error!("ACCEPTOR_ERR: {err}")
                }
            });

            Ok(())
        }
        .await;

        if let Some(err) = block.err() {
            log::error!("IDENTITY_ERR: {err}")
        }
    });
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityDestroy(
    _: JNIEnv,
    _class: JC,
    // Further Arguments
) {
    if let Some(tok) = CONN_CANCEL.lock().take() {
        tok.cancel();
    }
}
