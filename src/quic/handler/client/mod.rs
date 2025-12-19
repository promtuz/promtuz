use common::crypto::PublicKey;
use common::crypto::encrypt::Encrypted;
use common::crypto::get_nonce;
use common::crypto::get_shared_key;
use common::crypto::get_static_keypair;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::relay::HandshakePacket;
use common::msg::relay::MiscPacket;
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
                let (esk, server_epk) = get_static_keypair();
                let mut proof = None;

                while let Ok(packet_size) = recv.read_u32().await {
                    let mut packet = vec![0u8; packet_size as usize];
                    println!("GOT PACKET SIZE : {packet_size}");
                    if let Err(err) = recv.read_exact(&mut packet).await {
                        println!("Read failed : {}", err); // temp
                        break;
                    }
                    println!("PACKET: {:?}", packet);

                    use HandshakePacket::*;

                    let msg = HandshakePacket::from_cbor(&packet);

                    println!("MESSAGE: {:?}", msg);

                    if let Ok(ClientHello { ipk }) = msg {
                        println!("CLIENT HELLO: {:?}", ipk);

                        // Diffie hellman of our ephemeral static secret and their identity public
                        // key proof must be decrypted using our ephemeral
                        // public and their static secret this proves the
                        // ownership of their identity public key
                        let dh = esk.diffie_hellman(&PublicKey::from(ipk));

                        let key =
                            get_shared_key(dh.as_bytes(), &[0u8; 32], "handshake.challenge.key");

                        proof = Some(get_nonce::<16>());

                        println!("PROOF - {}", hex::encode(proof.unwrap()));

                        let ct = Encrypted::encrypt_once(
                            &proof.unwrap(),
                            &key,
                            &[&ipk[..], &server_epk.to_bytes()[..]].concat(),
                        );

                        let challenge =
                            ServerChallenge { epk: server_epk.to_bytes(), ct: ct.try_into().ok()? };

                        send.write_all(&frame_packet(&challenge.to_cbor().ok()?)).await.ok()?;
                        send.flush().await.ok()?;
                    } else if let Ok(ClientProof { proof: client_proof }) = msg
                        && let Some(proof) = proof
                    {
                        println!("SERVER PROOF : {:?}", proof);
                        println!("CLIENT PROOF : {:?}", client_proof);

                        if proof == client_proof {
                            println!("ACCEPTED");
                            let accept =
                                ServerAccept { timestamp: systime().as_secs() }.to_cbor().ok()?;
                            send.write_all(&frame_packet(&accept)).await.ok()?;
                            send.flush().await.ok()?;
                        } else {
                            println!("REJECTED");
                            let accept =
                                ServerReject { reason: "Proof Mismatch".into() }.to_cbor().ok()?;
                            send.write_all(&frame_packet(&accept)).await.ok()?;
                            send.flush().await.ok()?;
                        }

                        _ = send.finish();
                    } else if let Ok(MiscPacket::PubAddressReq { port: _ }) =
                        MiscPacket::from_cbor(&packet)
                    {
                        println!("GOT PUB ADDR REQ: {:?}", packet);
                        let pub_addr = addr.ip().to_string();
                        println!("SENT ADDR: {:?}", pub_addr);

                        let resp = MiscPacket::PubAddressRes { addr: pub_addr }.to_cbor().ok()?;

                        send.write_all(&frame_packet(&resp)).await.ok()?;
                        _ = send.finish();
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
