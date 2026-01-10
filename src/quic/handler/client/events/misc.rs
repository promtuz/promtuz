use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_rel::MiscP;
use common::proto::client_rel::RelayPacket;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;

pub(super) async fn handle_misc(packet: MiscP, ctx: ClientCtxHandle, tx: &mut SendStream) -> Result<()> {
    use MiscP::*;

    let ctx = ctx.read().await;

    match packet {
        PubAddressReq => {
            let addr = ctx.conn.remote_address();

            RelayPacket::Misc(PubAddressRes { addr: addr.ip() })
                .send(tx)
                .await
        },
        _ => Err(anyhow!("No")),
    }
}
