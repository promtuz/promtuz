use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::QueryResultP;
use common::proto::client_rel::SRelayPacket;
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
