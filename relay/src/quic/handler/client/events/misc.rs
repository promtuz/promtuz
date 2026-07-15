use anyhow::Result;
use common::debug;
use common::proto::Sender;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::QueryResultP;
use common::proto::client_rel::SRelayPacket;
use common::proto::dht_p2p::PushPseudonymPublish;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;

pub(super) async fn handle_misc(
    packet: QueryP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    use QueryP::*;
    use SRelayPacket::*;

    match packet {
        PubAddress => {
            let addr = ctx.conn.remote_address();

            use QueryResultP::*;

            QueryResult(PubAddress { addr }).send(tx).await.map_err(|e| e.into())
        },
    }
}

/// Store `IPK → P` so the DHT enqueue path can wake this device. Bound to the
/// connection-authenticated `ctx.ipk`; the client cannot register for another
/// IPK. Not cleared on disconnect (an offline device is exactly the one to
/// wake). Fire-and-forget — no reply.
pub(super) async fn handle_register_push(
    pseudonym: [u8; 32], timestamp: u64, sig: [u8; 64], ctx: ClientCtxHandle,
) -> Result<()> {
    let publish = PushPseudonymPublish {
        user_ipk: ctx.ipk.to_bytes().into(),
        pseudonym: pseudonym.into(),
        timestamp,
        user_sig: sig.into(),
    };
    if !crate::dht::push_replication::valid_publish(&publish, crate::util::systime().as_millis() as u64) {
        return Ok(());
    }
    // Keep this relay's local durable record for DHT-disabled deployments;
    // enabled DHT relays fan the exact user-signed record to target homes.
    ctx.relay.store.put_push_pseudonym(&ctx.ipk.to_bytes(), &pseudonym)?;
    ctx.relay.push_pseudonyms.write().insert(ctx.ipk.to_bytes(), pseudonym);
    if let Some(dht) = ctx.relay.dht.clone() {
        tokio::spawn(crate::dht::push_replication::replicate_to_homes(dht, publish));
    }
    debug!("client({}) registered push-pseudonym", ctx.conn.remote_address());
    Ok(())
}
