use std::net::SocketAddr;

use anyhow::Result;
use common::debug;
use common::proto::pack::{Packable, Packer, Unpacker};
use quinn::Connection;
use tokio::io::AsyncWriteExt;

use super::super::msg::dht::DhtRequest;
use super::super::msg::dht::DhtResponse;
use crate::dht::NodeContact;
use crate::dht::UserRecord;
use crate::quic::handler::Handler;
use crate::relay::RelayRef;
use crate::util::systime;

impl Packable for DhtRequest {}
impl Packable for DhtResponse {}

/// Send a single DHT request over a fresh peer connection.
pub async fn send_dht_request(
    relay: RelayRef, target: NodeContact, req: DhtRequest,
) -> Result<DhtResponse> {
    let (endpoint, cfg) = {
        let r = relay.lock().await;
        (r.endpoint.clone(), r.peer_client_cfg.clone())
    };
    let conn: Connection =
        endpoint.connect_with((*cfg).clone(), target.addr, &target.id.to_string())?.await?;

    let (mut send, mut recv) = conn.open_bi().await?;

    send.write_all(&req.pack()?).await?;
    send.flush().await?;

    let resp = DhtResponse::unpack(&mut recv).await?;
    Ok(resp)
}

/// Replicate a user record to k closest peers based on current routing table.
pub async fn replicate_user(relay: RelayRef, record: UserRecord) {
    let targets = {
        let dht = { relay.lock().await.dht.clone() };
        let dht = dht.read().await;
        dht.replication_targets(&record.ipk)
    };

    for target in targets {
        let relay_clone = relay.clone();
        let record_clone = record.clone();
        tokio::spawn(async move {
            let req = DhtRequest::StoreUser { record: record_clone };
            if let Err(err) = send_dht_request(relay_clone, target.clone(), req).await {
                println!("DHT: replicate to {} failed: {}", target.id, err);
            }
        });
    }
}

/// For handling incoming connection from other relays in network
impl Handler {
    pub async fn handle_peer(self, relay: RelayRef) {
        let conn = self.conn.clone();
        let remote_addr = conn.remote_address();
        debug!("connection from peer({remote_addr})");

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let relay = relay.clone();
            tokio::spawn(async move {
                loop {
                    let req = match DhtRequest::unpack(&mut recv).await {
                        Ok(req) => req,
                        Err(_) => break,
                    };
                    if let Err(err) =
                        handle_request(relay.clone(), req, &mut send, remote_addr).await
                    {
                        common::error!("failed to handle peer({remote_addr}) request: {err}");
                        let _ = send_error(&mut send, "internal error").await;
                    }
                }
            });
        }
    }
}

async fn handle_request(
    relay: RelayRef, req: DhtRequest, send: &mut (impl AsyncWriteExt + Unpin),
    _remote_addr: SocketAddr,
) -> Result<()> {
    match req {
        DhtRequest::Ping { from, addr } => {
            let now = systime().as_secs();
            {
                let dht = { relay.lock().await.dht.clone() };
                let mut dht = dht.write().await;
                dht.upsert_node(NodeContact { id: from, addr, last_seen: now });
            }
            let resp = DhtResponse::Pong { from: relay.lock().await.id };
            send.write_all(&resp.pack()?).await?;
            send.flush().await?;
        },
        DhtRequest::StoreUser { record } => {
            let ok = {
                let dht = { relay.lock().await.dht.clone() };
                let mut dht = dht.write().await;
                dht.upsert_user(record)
            };
            let resp = if ok {
                DhtResponse::StoreOk
            } else {
                DhtResponse::Error { reason: "store rejected".into() }
            };
            send.write_all(&resp.pack()?).await?;
            send.flush().await?;
        },
        DhtRequest::FindUser { ipk } => {
            let (record, nodes) = {
                let dht = { relay.lock().await.dht.clone() };
                let dht = dht.read().await;
                (dht.get_user(&ipk), dht.replication_targets(&ipk))
            };
            let resp = if let Some(rec) = record {
                DhtResponse::UserResult { records: vec![rec] }
            } else {
                DhtResponse::NodeResult { nodes }
            };
            send.write_all(&resp.pack()?).await?;
            send.flush().await?;
        },
        DhtRequest::FindNode { target } => {
            let dht = { relay.lock().await.dht.clone() };
            let nodes = { dht.read().await.get_closest_nodes(target, 8) };
            let resp = DhtResponse::NodeResult { nodes };
            send.write_all(&resp.pack()?).await?;
            send.flush().await?;
        },
    }
    Ok(())
}

async fn send_error(send: &mut (impl AsyncWriteExt + Unpin), reason: &str) -> Result<()> {
    let resp = DhtResponse::Error { reason: reason.to_string() };
    send.write_all(&resp.pack()?).await?;
    send.flush().await?;
    Ok(())
}
