use common::PROTOCOL_VERSION;
use common::crypto::get_nonce;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::relay::HandshakePacket;
use common::msg::relay::MiscPacket;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;
use crate::util::systime;

mod events;

fn frame_packet(packet: &[u8]) -> Vec<u8> {
    let size: [u8; 4] = (packet.len() as u32).to_be_bytes();
    [&size, packet].concat()
}

impl Handler {
    pub async fn handle_client(self, relay: RelayRef) {
        let conn = self.conn.clone();
        let addr = conn.remote_address();

        println!("CLIENT: CONN({})", self.conn.remote_address());

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            println!("CLIENT: OPENED BI STREAM");

            tokio::spawn(async move {
                // TEMPORARY; USE EPHEMERAL WITH DYNAMIC DATA STORAGE N SHI
                // let (esk, server_epk) = get_static_keypair();
                let nonce = get_nonce::<32>();
                let mut user_ipk = None;

                while let Ok(packet_size) = recv.read_u32().await {
                    let mut packet = vec![0u8; packet_size as usize];
                    if let Err(err) = recv.read_exact(&mut packet).await {
                        break;
                    }

                    use HandshakePacket::*;

                    let msg = HandshakePacket::from_cbor(&packet);

                    println!("MESSAGE: {:?}", msg);

                    if let Ok(ClientHello { ipk }) = msg {
                        println!("CLIENT HELLO: {:?}", ipk);

                        user_ipk = Some(VerifyingKey::from_bytes(&ipk).ok()?);

                        let challenge = ServerChallenge { nonce };

                        send.write_all(&frame_packet(&challenge.to_cbor().ok()?)).await.ok()?;
                        send.flush().await.ok()?;
                    } else if let Ok(ClientProof { sig }) = msg
                        && let Some(ipk) = user_ipk
                    {
                        let msg =
                            [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &nonce]
                                .concat();

                        let packet = if Signature::from_slice(&sig)
                            .and_then(|sig| ipk.verify_strict(&msg, &sig))
                            .is_ok()
                        {
                            ServerAccept { timestamp: systime().as_secs() }
                        } else {
                            ServerReject { reason: "Invalid Signature".into() }
                        };

                        send.write_all(&frame_packet(&packet.to_cbor().ok()?)).await.ok()?;
                        send.flush().await.ok()?;
                        _ = send.finish();
                    } else if let Ok(MiscPacket::PubAddressReq) = MiscPacket::from_cbor(&packet) {
                        let resp = MiscPacket::PubAddressRes { addr: addr.ip() }.to_cbor().ok()?;

                        send.write_all(&frame_packet(&resp)).await.ok()?;
                        send.flush().await.ok()?;
                    } else {
                        // println!("PACKET: {:?}", packet);
                    }
                }
                Some(())
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            println!("CLIENT({}): CLOSE({})", self.conn.remote_address(), close_reason);
        }
    }
}
