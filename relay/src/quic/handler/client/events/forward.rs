use anyhow::Result;
use common::crypto::get_nonce;
use common::info;
use common::proto::Sender;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::ForwardP;
use common::proto::client_rel::ForwardResultP;
use common::proto::client_rel::SRelayPacket;
use common::proto::pack::Packer;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::util::systime;

pub(super) async fn handle_forward(
    fwd: ForwardP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // 1. Verify signature: sender must prove authorship
    let sig_valid = (|| {
        let vk = VerifyingKey::from_bytes(&fwd.from).ok()?;
        let sig = Signature::from_slice(&*fwd.sig).ok()?;
        let msg = [fwd.to.as_slice(), fwd.from.as_slice(), &fwd.payload].concat();
        vk.verify_strict(&msg, &sig).ok()
    })();

    if sig_valid.is_none() {
        SRelayPacket::ForwardResult(ForwardResultP::InvalidSig).send(tx).await?;
        return Ok(());
    }

    let (recipient, delivery) = {
        let ForwardP { to, from, payload, sig } = fwd;
        (to, DeliverP { from, payload, sig })
    };

    // 2. Check if recipient is connected locally
    let recipient_conn = { ctx.relay.clients.read().get(&*recipient).cloned() };

    if let Some(conn) = recipient_conn {
        // Deliver locally: open a stream to the recipient and send the packet
        match conn.open_bi().await {
            Ok((mut deliver_tx, _)) => {
                SRelayPacket::Deliver(delivery).send(&mut deliver_tx).await?;
                SRelayPacket::ForwardResult(ForwardResultP::Accepted).send(tx).await?;
            },
            Err(e) => {
                info!("FORWARD: failed to open stream to recipient: {e}");
                SRelayPacket::ForwardResult(ForwardResultP::Error {
                    reason: "delivery failed".into(),
                })
                .send(tx)
                .await?;
            },
        }
    } else {
        // // TODO: DHT lookup → forward to recipient's relay (cross-relay routing)
        info!("FORWARD: recipient {} not connected locally", hex::encode(recipient));

        let packet = SRelayPacket::Deliver(delivery).pack()?;

        let timestamp: [u8; 8] = (systime().as_millis() as u64).to_be_bytes();
        let rand = get_nonce::<4>();

        let mut key = [0u8; 44];
        key[..32].copy_from_slice(&*recipient);
        key[32..40].copy_from_slice(&timestamp);
        key[40..44].copy_from_slice(&rand);

        ctx.relay.rocks.put(key, packet)?;

        SRelayPacket::ForwardResult(ForwardResultP::Accepted).send(tx).await?;
    }

    Ok(())
}
