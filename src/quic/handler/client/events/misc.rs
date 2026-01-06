use anyhow::Result;
use anyhow::anyhow;
use common::msg::relay::MiscP;

use crate::quic::handler::client::ClientCtxHandle;

pub(super) async fn handle_misc(packet: MiscP, ctx: ClientCtxHandle) -> Result<()> {
    use MiscP::*;
    match packet {
        PubAddressRes { addr } => {
            todo!("")
        },
        _ => return Err(anyhow!("No")),
    }

    Ok(())
}
