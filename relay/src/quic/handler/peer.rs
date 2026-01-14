use std::net::SocketAddr;

use anyhow::Result;
use common::proto::pack::Unpacker;
use quinn::Connection;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::super::msg::dht::DhtRequest;
use super::super::msg::dht::DhtResponse;
use crate::dht::NodeContact;
use crate::dht::UserRecord;
use crate::quic::handler::Handler;
use crate::relay::RelayRef;
use crate::util::systime;

async fn read_framed(recv: &mut (impl AsyncReadExt + Unpin)) -> Option<Vec<u8>> {
    let len = recv.read_u32().await.ok()?;
    let mut buf = vec![0u8; len as usize];
    recv.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

/// Send a single DHT request over a fresh peer connection.
pub async fn send_dht_request(
    relay: RelayRef, target: NodeContact, _req: DhtRequest,
) -> Result<DhtResponse> {
    let (endpoint, cfg) = {
        let r = relay.lock().await;
        (r.endpoint.clone(), r.peer_client_cfg.clone())
    };
    let conn: Connection =
        endpoint.connect_with((*cfg).clone(), target.addr, &target.id.to_string())?.await?;

    let (mut _send, mut recv) = conn.open_bi().await?;

    // TODO: 
    // send.write_all(&frame_packet(&req.to_cbor()?)).await?;
    // send.flush().await?;

    if let Some(bytes) = read_framed(&mut recv).await {
        let resp = DhtResponse::from_cbor(&bytes)?;
        Ok(resp)
    } else {
        anyhow::bail!("empty response from peer");
    }
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
        println!("PEER: CONN({})", remote_addr);

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let relay = relay.clone();
            tokio::spawn(async move {
                while let Some(packet) = read_framed(&mut recv).await {
                    let Ok(req) = DhtRequest::from_cbor(&packet) else {
                        let _ = send_error(&mut send, "bad packet").await;
                        continue;
                    };
                    if let Err(err) =
                        handle_request(relay.clone(), req, &mut send, remote_addr).await
                    {
                        println!("PEER: handler error: {err}");
                        let _ = send_error(&mut send, "internal error").await;
                    }
                }
                Some(())
            });
        }
    }
}

#[allow(unused)]
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
            todo!();
            // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
            // send.flush().await?;
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
            todo!();
            // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
            // send.flush().await?;
        },
        DhtRequest::FindUser { ipk } => {
            let (record, nodes) = {
                let dht = { relay.lock().await.dht.clone() };
                let dht = dht.read().await;
                (dht.get_user(&ipk), dht.replication_targets(&ipk))
            };
            todo!();
            if let Some(rec) = record {
                let resp = DhtResponse::UserResult { records: vec![rec] };
                // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
            } else {
                let resp = DhtResponse::NodeResult { nodes };
                // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
            }
            send.flush().await?;
        },
        DhtRequest::FindNode { target } => {
            let dht = { relay.lock().await.dht.clone() };
            let nodes = { dht.read().await.get_closest_nodes(target, 8) };
            let resp = DhtResponse::NodeResult { nodes };
            todo!();
            // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
            // send.flush().await?;
        },
    }
    Ok(())
}

async fn send_error(_send: &mut (impl AsyncWriteExt + Unpin), reason: &str) -> Result<()> {
    let _resp = DhtResponse::Error { reason: reason.to_string() };
    todo!();
    // send.write_all(&frame_packet(&resp.to_cbor()?)).await?;
    // send.flush().await?;
    // Ok(())
}
