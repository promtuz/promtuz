use std::sync::Arc;

use common::debug;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::GatewayRequest;
use common::proto::push::PushProvider;
use common::proto::push::RegisterResponse;
use common::proto::push::WakeRequest;
use common::proto::sticker::StoreReject;
use common::proto::sticker::StoreResponse;
use common::quic::protorole::ProtoRole;
use common::server::accept::quota;
use common::warn;
use governor::RateLimiter;
use quinn::Connection;

use crate::gateway::Gateway;

const CLIENT_RPC_PER_MIN: u32 = 300;
const RELAY_RPC_PER_MIN: u32 = 1200;
const RPC_BURST: u32 = 300;

/// One request per bi-stream. `Register` and `Store` reply; `Wake` does not.
pub struct Handler;

impl Handler {
    pub async fn handle(conn: Connection, gateway: Arc<Gateway>) {
        let addr = conn.remote_address();

        let role = match ProtoRole::from_conn(&conn) {
            Some(role @ (ProtoRole::Client | ProtoRole::Relay)) => role,
            Some(_) => return conn.close(0u32.into(), b"UnsupportedALPN"),
            None => return conn.close(0u32.into(), b"NoALPN"),
        };

        let per_minute =
            if role == ProtoRole::Relay { RELAY_RPC_PER_MIN } else { CLIENT_RPC_PER_MIN };
        let limiter = RateLimiter::direct(quota(per_minute, RPC_BURST));
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            limiter.until_ready().await;
            let gateway = gateway.clone();
            tokio::spawn(async move {
                match GatewayRequest::unpack(&mut recv).await {
                    Ok(GatewayRequest::Store(_)) if role != ProtoRole::Client => {
                        warn!("gateway: store request from a non-device {addr}; ignored");
                    },
                    Ok(GatewayRequest::Store(req)) => {
                        let resp = match &gateway.store {
                            Some(store) => store.handle(req).await,
                            None => StoreResponse::Rejected(StoreReject::Unavailable),
                        };
                        match resp.pack() {
                            Ok(bytes) => {
                                if let Err(e) = send.write_all(&bytes).await {
                                    debug!("gateway: store response to {addr} not written: {e}");
                                }
                                let _ = send.finish();
                            },
                            Err(e) => warn!("gateway: store response pack failed: {e}"),
                        }
                    },
                    Ok(GatewayRequest::Register(reg)) => {
                        let response = match gateway.registry.register(&reg) {
                            // A pseudonym is never journalled alongside an address.
                            Ok(()) => {
                                debug!(
                                    "gateway: registered {:?} token for P={}",
                                    reg.provider,
                                    hex::encode(&reg.pseudonym.0[..8])
                                );
                                RegisterResponse::Registered
                            },
                            Err(e) => {
                                warn!("gateway: rejected registration from {addr}: {e}");
                                RegisterResponse::Rejected
                            },
                        };
                        if let Ok(bytes) = response.pack() {
                            let _ = send.write_all(&bytes).await;
                            let _ = send.finish();
                        }
                    },
                    Ok(GatewayRequest::Wake(_)) if role != ProtoRole::Relay => {
                        warn!("gateway: wake from a non-relay {addr}; ignored");
                    },
                    Ok(GatewayRequest::Wake(req)) => Self::dispatch_wake(&gateway, req).await,
                    Err(e) => warn!("gateway: request decode failed from {addr}: {e}"),
                }
            });
        }
    }

    async fn dispatch_wake(gateway: &Gateway, req: WakeRequest) {
        let p = hex::encode(&req.pseudonym.0[..8]);
        // A wake carries nothing: the device fetches the message from its relay, and any bytes
        // here would reach a phone under the gateway's FCM credentials.
        if !req.payload.is_empty() {
            warn!("gateway: wake for P={p} carried a payload; dropped");
            return;
        }
        if gateway.wakes.check_key(&req.pseudonym.0).is_err() {
            debug!("gateway: wake budget exhausted for P={p}; dropped");
            return;
        }
        let entry = match gateway.registry.resolve(&req.pseudonym.0) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                warn!("gateway: wake for unknown P={p}");
                return;
            },
            Err(e) => {
                warn!("gateway: push registry lookup failed: {e:#}");
                return;
            },
        };
        match entry.provider {
            PushProvider::Fcm => {
                let Some(fcm) = &gateway.fcm else {
                    warn!("gateway: FCM token but FCM not configured");
                    return;
                };
                let token = String::from_utf8_lossy(&entry.token);
                match fcm.send(token.as_ref(), &req.payload, req.class).await {
                    Ok(()) => debug!("gateway: FCM wake pushed for P={p}"),
                    Err(e) => warn!("gateway: FCM dispatch failed: {e:#}"),
                }
            },
            other => warn!("gateway: {other:?} dispatch not implemented"),
        }
    }
}
