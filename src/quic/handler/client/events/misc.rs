use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_rel::MiscP;
use common::proto::client_rel::RelayPacket;

use crate::quic::handler::client::ClientCtxHandle;

pub(super) async fn handle_misc(packet: MiscP, ctx: ClientCtxHandle) -> Result<()> {
    use MiscP::*;

    let mut ctx = ctx.write().await;

    match packet {
        PubAddressReq => {
            let addr = ctx.conn.remote_address();

            RelayPacket::Misc(PubAddressRes { addr: addr.ip() })
                .send(ctx.send.as_mut().unwrap())
                .await
        },
        _ => Err(anyhow!("No")),
    }
}
