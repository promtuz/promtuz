use anyhow::Result;
use common::debug;
use common::proto::Sender;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::QueryResultP;
use common::proto::client_rel::SRelayPacket;
use common::proto::dht_p2p::PushPseudonymPublish;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::spawn_tied;

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

pub(super) async fn handle_turn_credentials(ctx: ClientCtxHandle, tx: &mut SendStream) -> Result<()> {
    let creds = ctx
        .relay
        .turn
        .as_ref()
        .map(|t| t.credentials(&ctx.ipk.to_bytes(), common::utils::now_ms()));
    SRelayPacket::TurnCredentials(creds).send(tx).await.map_err(|e| e.into())
}

/// The pseudonym survives disconnect: an offline device is exactly the one to wake.
pub(super) async fn handle_register_push(
    pseudonym: [u8; 32], timestamp: u64, sig: [u8; 64], ctx: ClientCtxHandle,
) -> Result<()> {
    if ctx.limits.register_push.check().is_err() {
        return Ok(());
    }
    let ipk = ctx.ipk.to_bytes();
    let publish = PushPseudonymPublish {
        user_ipk: ipk.into(),
        pseudonym: pseudonym.into(),
        timestamp,
        user_sig: sig.into(),
    };
    if !crate::dht::push_replication::valid_publish(&publish, common::utils::now_ms()) {
        return Ok(());
    }
    let store = ctx.relay.store.clone();
    tokio::task::spawn_blocking(move || store.put_push_pseudonym(&ipk, &pseudonym)).await??;
    if let Some(dht) = ctx.relay.dht.clone() {
        spawn_tied(&ctx.cancel, crate::dht::push_replication::replicate_to_homes(dht, publish));
    }
    debug!("client({}) registered push-pseudonym", ctx.conn.remote_address());
    Ok(())
}
