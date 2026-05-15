use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::Sender;
use common::proto::client_peer::ClientPeerPacket;
use common::proto::client_peer::IdentityP;
use common::proto::pack::Unpacker;
use jni::JNIEnv;
use jni::objects::JObject;
use jni::objects::JValue;
use jni_macro::jni;
use log::debug;
use log::info;
use log::warn;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

mod qr;
mod scanner;

use crate::ENDPOINT;
use crate::JC;
use crate::JVM;
use crate::RUNTIME;
use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::idqr::IdentityQr;
use crate::events::Emittable;
use crate::events::identity::IdentityEv;
// Identity-key separation (Group A): the cert SPKI is now the peer's TLS
// sub-key, not their long-term IPK. The IPK is carried in `IdentityP` and
// bound to the TLS sub-key by `verify_ipk_binding`. Group D should keep the
// `ipk` field in `PendingIdentity` populated from the *verified* IPK rather
// than the cert SPKI.
use crate::data::identity::IdentitySigner;
use crate::quic::peer_config::extract_peer_tls_pubkey;
use crate::quic::peer_config::ipk_binding_message;
use crate::quic::peer_config::verify_ipk_binding;
use crate::quic::server::RELAY;

static CONN_CANCEL: Lazy<Mutex<Option<CancellationToken>>> = Lazy::new(|| Mutex::new(None));

/// Single pending identity request (only one at a time for simpler flow)
static PENDING_IDENTITY: Lazy<Mutex<Option<PendingIdentity>>> = Lazy::new(|| Mutex::new(None));

struct PendingIdentity {
    respond: oneshot::Sender<bool>,
    // Kept for potential diagnostic logging on the accept/reject path; not
    // currently read because the responder only needs `respond` and `name`.
    #[allow(dead_code)]
    ipk: [u8; 32],
    name: String,
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityInit(env: JNIEnv, _: JC, class: JObject) {
    let class = env.new_global_ref(class).unwrap();

    let identity = Identity::get().expect("identity not found");

    RUNTIME.spawn(async move {
        let block: Result<()> = async move {
            let relay = {
                let g = RELAY.read();
                g.clone().unwrap()
            };
            let conn = relay.connection.clone().ok_or(anyhow!("relay is not connected!"))?;

            // we advertise our ACTUAL public address including port as NAT can port forward
            let addr = relay.public_addr().await?;

            let qr = IdentityQr { ipk: identity.ipk(), addr, name: identity.name() };

            let jvm = JVM.get().unwrap();
            let mut env = jvm.attach_current_thread().unwrap();
            let qr_arr = &env.byte_array_from_slice(&qr.to_bytes()).unwrap();
            env.call_method(&class, "onQRCreate", "([B)V", &[JValue::Object(qr_arr)]).unwrap();
            drop(env);

            let ep = ENDPOINT.get().unwrap().clone();

            let token = CancellationToken::new();
            {
                let mut guard = CONN_CANCEL.lock();
                if let Some(old) = guard.take() {
                    old.cancel();
                }
                *guard = Some(token.clone())
            }

            // conn.closed()

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => {
                            info!("IDENTITY: conn loop cancelled");
                            // Clean up pending identity
                            *PENDING_IDENTITY.lock() = None;
                            break;
                        }
                        reason = conn.closed() => {
                            let mut env = jvm.attach_current_thread().unwrap();
                            
                            // clearing out the qr code
                            env.call_method(&class, "onQRCreate", "([B)V", &[JValue::Object(&JObject::null())]).ok();

                            warn!("WARN: connection closed: {reason}")
                        }
                        incoming = ep.accept() => {
                            let Some(incoming) = incoming else { continue };

                            // Spawn a task for each connection
                            // let km = km.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_identity_connection(incoming).await {
                                    warn!("IDENTITY: connection handler error: {e}");
                                }
                            });
                        }
                    }
                }
            });

            Ok(())
        }
        .await;

        if let Some(err) = block.err() {
            log::error!("ERROR: failed to maintain identity server: {err}")
        }
    });
}

async fn handle_identity_connection(incoming: quinn::Incoming) -> Result<()> {
    let conn = incoming.await.map_err(|e| anyhow!("failed to accept incoming conn: {e}"))?;
    let (mut tx, mut recv) =
        conn.accept_bi().await.map_err(|e| anyhow!("failed to accept stream: {e}"))?;

    use ClientPeerPacket::*;
    use IdentityP::*;

    // Wait for the AddMe packet
    let Identity(ipack) =
        ClientPeerPacket::unpack(&mut recv).await.map_err(|e| anyhow!("failed to unpack: {e}"))?;

    match ipack {
        AddMe { name, ipk, ipk_sig } => {
            // The cert SPKI is the peer's TLS sub-key, not their IPK. We
            // accept the IPK from the application packet only after
            // verifying the IPK has signed the TLS sub-key it just used to
            // complete the handshake — that proves the peer presenting the
            // cert really controls the long-term IPK.
            let tls_pubkey = extract_peer_tls_pubkey(&conn)
                .ok_or_else(|| anyhow!("failed to extract peer TLS pubkey from certificate"))?;
            verify_ipk_binding(&ipk, &tls_pubkey, &ipk_sig)
                .map_err(|e| anyhow!("peer IPK<->TLS binding rejected: {e}"))?;

            info!("{name} says add me.\nIPK({})", hex::encode(ipk));

            // Atomically check-and-set the pending slot. Doing this in two
            // separate `lock()` calls (check, then set) is a TOCTOU race: two
            // inbound peers can both observe `is_some() == false`, both create
            // their own oneshot, and the second `Some(..)` would silently drop
            // the first peer's `respond` Sender — making its `decision_rx`
            // resolve to `Err(Cancelled)` instead of "busy". That confuses the
            // first peer (its task sends "cancelled" rather than holding the
            // happy path), and it's purely an artifact of releasing the lock
            // between the two operations. Combining them under a single
            // critical section makes "first one wins, others see busy"
            // unambiguous.
            let (decision_tx, decision_rx) = oneshot::channel();
            let already_pending = {
                let mut pending = PENDING_IDENTITY.lock();
                if pending.is_some() {
                    true
                } else {
                    *pending = Some(PendingIdentity {
                        respond: decision_tx,
                        ipk,
                        name: name.clone(),
                    });
                    false
                }
            };

            if already_pending {
                // First-arrival lost: tell this peer we're busy and tear down.
                // `decision_tx` was never installed, so it drops here, which
                // is fine — no awaiter is watching it.
                warn!("IDENTITY: already have pending request, rejecting {name}");
                ClientPeerPacket::Identity(No { reason: "busy".to_string() }).send(&mut tx).await?;
                return Ok(());
            }

            // Emit event to Android (this will hide the QR and show the request card)
            IdentityEv::AddMe { ipk, name: name.clone() }.emit();

            // Wait for decision with timeout (60 seconds)
            let decision = timeout(Duration::from_secs(60), decision_rx).await;

            // Send response based on decision
            match decision {
                Ok(Ok(true)) => {
                    info!("IDENTITY: {name} accepted");

                    // Build the IPK<->TLS-subkey binding for our own response.
                    // We sign with our long-term IPK over the canonical
                    // transcript that names *our* TLS sub-key pubkey, derived
                    // identically on both sides via HKDF. The receiver verifies
                    // against the TLS sub-key it observed in our cert.
                    let our_tls_subkey = IdentitySigner::tls_subkey()
                        .map_err(|e| anyhow!("failed to derive our TLS sub-key: {e}"))?;
                    let binding_msg =
                        ipk_binding_message(&our_tls_subkey.verifying_key().to_bytes());
                    let (our_ipk_sig, our_ipk) = IdentitySigner::sign_with_ipk(&binding_msg)
                        .map_err(|e| anyhow!("failed to sign IPK binding: {e}"))?;

                    ClientPeerPacket::Identity(AddedYou {
                        ipk: our_ipk,
                        ipk_sig: our_ipk_sig.to_bytes(),
                    })
                    .send(&mut tx)
                    .await?;

                    // Wait for scanner to confirm they saved the contact
                    match timeout(Duration::from_secs(15), ClientPeerPacket::unpack(&mut recv)).await {
                        Ok(Ok(Identity(Confirmed))) => {
                            info!("IDENTITY: {name} confirmed");

                            match Contact::save(ipk, name.clone()) {
                                Ok(_) => info!("IDENTITY: saved contact {name}"),
                                Err(e) => warn!("IDENTITY: failed to save contact {name}: {e}"),
                            }
                        },
                        Ok(Ok(other)) => {
                            warn!("IDENTITY: unexpected packet from {name}: {other:?}");
                        },
                        Ok(Err(e)) => {
                            warn!("IDENTITY: failed to read confirmation from {name}: {e}");
                        },
                        Err(_) => {
                            warn!("IDENTITY: {name} did not confirm in time, not saving contact");
                        },
                    }
                },
                Ok(Ok(false)) => {
                    info!("IDENTITY: {name} rejected");
                    ClientPeerPacket::Identity(No { reason: "rejected".to_string() })
                        .send(&mut tx)
                        .await?;
                },
                Ok(Err(_)) => {
                    debug!("IDENTITY: {name} decision channel closed");
                    ClientPeerPacket::Identity(No { reason: "cancelled".to_string() })
                        .send(&mut tx)
                        .await?;
                },
                Err(_) => {
                    warn!("IDENTITY: {name} timed out waiting for decision");
                    ClientPeerPacket::Identity(No { reason: "timeout".to_string() })
                        .send(&mut tx)
                        .await?;
                    // Clean up
                    *PENDING_IDENTITY.lock() = None;
                },
            }
        },
        NeverMind {  } => {
            todo!("Implement cancellation of request box")
        },
        _ => { /* mustn't reach this */}
    } 

    Ok(())
}

/// Called from Android to accept an identity request
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityAccept(_: JNIEnv, _: JC) {
    respond_to_identity(true);
}

/// Called from Android to reject an identity request
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityReject(_: JNIEnv, _: JC) {
    respond_to_identity(false);
}

fn respond_to_identity(accepted: bool) {
    if let Some(pending) = PENDING_IDENTITY.lock().take() {
        let _ = pending.respond.send(accepted);
        debug!(
            "IDENTITY: responded {} to {}",
            if accepted { "accept" } else { "reject" },
            pending.name
        );
    } else {
        warn!("IDENTITY: no pending identity found");
    }
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn identityDestroy(_: JNIEnv, _class: JC) {
    if let Some(tok) = CONN_CANCEL.lock().take() {
        tok.cancel();
    }
    // Clear pending identity
    *PENDING_IDENTITY.lock() = None;
}
