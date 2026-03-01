use anyhow::Result;
use common::proto::client_rel::DeliverP;
use common::proto::pack::Unpacker;
use quinn::SendStream;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use crate::quic::handler::client::ClientCtxHandle;

#[derive(Debug, KnownLayout, FromBytes, IntoBytes, Immutable)]
#[repr(C, packed)]
pub struct MessageKey {
    pub recipient: [u8; 32],
    pub timestamp: [u8; 8],
    pub rand: [u8; 4],
}

/// Sends all pending messages to user, waits for ack then clears the queue
///
/// Queue should not be cleared immediately as client might not have received everything,
/// due to network issues perhaps
pub(super) async fn handle_drain(ctx: ClientCtxHandle, tx: &mut SendStream) -> Result<()> {
    let queue = ctx.relay.rocks.prefix_iterator(ctx.ipk);

    // idk how it performs under load
    let messages: Vec<(MessageKey, DeliverP)> = queue
        .filter_map(|entry| {
            let (key, value) = entry.ok()?;
            let key = MessageKey::read_from_bytes(&key).ok()?;
            let msg = DeliverP::deser(&value).ok()?;
            Some((key, msg))
        })
        .collect();

    println!("MSGS: {messages:?}");

    Ok(())
}
