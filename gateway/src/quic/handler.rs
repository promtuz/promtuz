use std::sync::Arc;

use common::debug;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::GatewayRequest;
use common::proto::push::PushProvider;
use common::proto::push::WakeRequest;
use common::proto::sticker::StoreReject;
use common::proto::sticker::StoreResponse;
use common::quic::protorole::ProtoRole;
use common::warn;
use quinn::Connection;

use crate::gateway::Gateway;

/// Per-connection handler. Serves the one-RPC-per-bi-stream contract (mirrors
/// the resolver's client handler): each accepted bi-stream is one
/// [`GatewayRequest`], dispatched on its own task so a slow send can't
/// head-of-line block the connection's other streams.
///
/// `Register` (devices, `client/5`) verifies + stores `P → token`. `Wake`
/// (home relays, `relay/5`) resolves `P → token` and pushes it. `Store`
/// (devices) writes a sticker-pack object and is the one request answered on
/// its stream.
pub struct Handler;

impl Handler {
    pub async fn handle(conn: Connection, gateway: Arc<Gateway>) {
        let addr = conn.remote_address();

        // Only devices (`client/5`, registration) and home relays (`relay/5`,
        // wake) talk to the gateway. Anything else is closed, and each role
        // gets its own verb: a device registers, a relay wakes.
        let role = match ProtoRole::from_conn(&conn) {
            Some(role @ (ProtoRole::Client | ProtoRole::Relay)) => role,
            Some(_) => return conn.close(0u32.into(), b"UnsupportedALPN"),
            None => return conn.close(0u32.into(), b"NoALPN"),
        };

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
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
                    Ok(GatewayRequest::Register(reg)) => match gateway.registry.register(&reg) {
                        // A pseudonym is never journalled alongside an address.
                        Ok(()) => debug!(
                            "gateway: registered {:?} token for P={}",
                            reg.provider,
                            hex::encode(&reg.pseudonym.0[..8])
                        ),
                        Err(e) => warn!("gateway: rejected registration from {addr}: {e}"),
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
        // A wake carries nothing: the message is waiting at the relay, and the
        // device fetches it there. Bytes offered here would be forwarded under
        // the gateway's FCM credentials to a phone that never asked for them.
        if !req.payload.is_empty() {
            warn!("gateway: wake for P={p} carried a payload; dropped");
            return;
        }
        if gateway.wakes.check_key(&req.pseudonym.0).is_err() {
            debug!("gateway: wake budget exhausted for P={p}; dropped");
            return;
        }
        let Some(entry) = gateway.registry.resolve(&req.pseudonym.0) else {
            warn!("gateway: wake for unknown P={p} — device never registered this pseudonym (stale/rotated P?)");
            return;
        };
        match entry.provider {
            PushProvider::Fcm => {
                let Some(fcm) = &gateway.fcm else {
                    warn!("gateway: FCM token but FCM not configured");
                    return;
                };
                let token = String::from_utf8_lossy(&entry.token);
                match fcm.send(token.as_ref(), &req.payload).await {
                    Ok(()) => debug!("gateway: FCM wake pushed for P={p}"),
                    Err(e) => warn!("gateway: FCM dispatch failed: {e:#}"),
                }
            },
            other => warn!("gateway: {other:?} dispatch not implemented"),
        }
    }
}
