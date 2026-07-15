use std::net::SocketAddr;
use std::sync::Arc;

use common::debug;
use common::proto::pack::Unpacker;
use common::proto::push::PushProvider;
use common::proto::push::PushRequest;
use common::proto::push::WakeRequest;
use common::quic::protorole::ProtoRole;
use common::warn;
use quinn::Connection;

use crate::gateway::Gateway;

/// Per-connection handler. Serves the one-RPC-per-bi-stream contract (mirrors
/// the resolver's client handler): each accepted bi-stream is one
/// [`PushRequest`], dispatched on its own task so a slow send can't
/// head-of-line block the connection's other streams.
///
/// `Register` (devices, `client/1`) verifies + stores `P → token`. `Wake`
/// (home relays, `relay/1`) resolves `P → token` and pushes it.
pub struct Handler;

impl Handler {
    pub async fn handle(conn: Connection, gateway: Arc<Gateway>) {
        let addr = conn.remote_address();
        debug!("incoming conn from {addr}");

        // Only devices (`client/1`, registration) and home relays (`relay/1`,
        // wake) talk to the gateway. Anything else is closed.
        match ProtoRole::from_conn(&conn) {
            Some(ProtoRole::Client | ProtoRole::Relay) => {},
            Some(_) => return conn.close(0u32.into(), b"UnsupportedALPN"),
            None => return conn.close(0u32.into(), b"NoALPN"),
        }

        while let Ok((_send, mut recv)) = conn.accept_bi().await {
            let gateway = gateway.clone();
            tokio::spawn(async move {
                match PushRequest::unpack(&mut recv).await {
                    Ok(PushRequest::Register(reg)) => match gateway.registry.register(&reg) {
                        Ok(()) => debug!("gateway: registered pseudonym from {addr}"),
                        Err(e) => warn!("gateway: rejected registration from {addr}: {e}"),
                    },
                    Ok(PushRequest::Wake(req)) => Self::dispatch_wake(&gateway, req, addr).await,
                    Err(e) => warn!("gateway: request decode failed from {addr}: {e}"),
                }
            });
        }
    }

    async fn dispatch_wake(gateway: &Gateway, req: WakeRequest, addr: SocketAddr) {
        let Some(entry) = gateway.registry.resolve(&req.pseudonym.0) else {
            warn!("gateway: wake for unknown pseudonym from {addr}");
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
                    Ok(()) => debug!("gateway: woke pseudonym via FCM ({} B)", req.payload.len()),
                    Err(e) => warn!("gateway: FCM dispatch failed: {e:#}"),
                }
            },
            other => warn!("gateway: {other:?} dispatch not implemented"),
        }
    }
}
