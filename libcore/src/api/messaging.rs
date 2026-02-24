use anyhow::anyhow;
use common::crypto::encrypt::Encrypted;
use common::proto::client_rel::ForwardP;
use common::proto::client_rel::ForwardResult;
use common::proto::client_rel::RelayPacket;
use common::proto::pack::Unpacker;
use jni::JNIEnv;
use jni::objects::JByteArray;
use jni::objects::JString;
use jni_macro::jni;
use log::error;
use log::info;

use crate::JC;
use crate::RUNTIME;
use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::quic::server::RELAY;

/// Decode a flat payload back into nonce + cipher.
pub fn decode_encrypted(payload: &[u8]) -> Option<Encrypted> {
    if payload.len() < 12 {
        return None;
    }
    Some(Encrypted { nonce: payload[..12].to_vec(), cipher: payload[12..].to_vec() })
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn sendMessage(mut env: JNIEnv, _: JC, to_ipk: JByteArray, content: JString) {
    let to: [u8; 32] = {
        let bytes = env.convert_byte_array(to_ipk).unwrap();
        bytes.try_into().unwrap()
    };

    let content: String = env.get_string(&content).unwrap().into();

    RUNTIME.spawn(async move {
        if let Err(e) = send_message_inner(to, content).await {
            error!("MESSAGE: send failed: {e}");
            MessageEv::Failed { to, reason: e.to_string() }.emit();
        }
    });
}

async fn send_message_inner(to: [u8; 32], content: String) -> anyhow::Result<()> {
    // 1. Look up contact and derive per-friendship shared key
    let contact = Contact::get(&to).ok_or_else(|| anyhow!("recipient not in contacts"))?;
    let shared_key = contact.shared_key()?;

    // 2. Encrypt
    let encrypted = Encrypted::encrypt(content.as_bytes(), &shared_key, &to);
    let payload = encrypted.flat();

    // 3. Get our IPK and sign
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();

    let sig_message = [to.as_slice(), our_ipk.as_slice(), &payload].concat();
    let sig = {
        let isk = Identity::secret_key_bytes();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&isk);
        use ed25519_dalek::Signer;
        signing_key.sign(&sig_message).to_bytes()
    };

    let fwd = ForwardP { to, from: our_ipk, payload, sig };

    // 4. Send to relay
    let conn = {
        let relay = RELAY.read();
        relay
            .as_ref()
            .and_then(|r| r.connection.clone())
            .ok_or_else(|| anyhow!("not connected to relay"))?
    };

    let (mut send, mut recv) = conn.open_bi().await?;
    RelayPacket::Forward(fwd).send(&mut send).await?;
    send.finish()?;

    // 5. Wait for result
    match RelayPacket::unpack(&mut recv).await? {
        RelayPacket::ForwardResult(ForwardResult::Accepted) => {
            info!("MESSAGE: sent to {}", hex::encode(to));
            MessageEv::Sent { to }.emit();
        },
        RelayPacket::ForwardResult(ForwardResult::NotFound) => {
            MessageEv::Failed { to, reason: "recipient not found".into() }.emit();
        },
        RelayPacket::ForwardResult(ForwardResult::InvalidSig) => {
            MessageEv::Failed { to, reason: "invalid signature".into() }.emit();
        },
        RelayPacket::ForwardResult(ForwardResult::Error { reason }) => {
            MessageEv::Failed { to, reason }.emit();
        },
        other => {
            MessageEv::Failed { to, reason: format!("unexpected: {other:?}") }.emit();
        },
    }

    Ok(())
}
