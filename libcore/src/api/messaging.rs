use anyhow::anyhow;
use common::crypto::encrypt::Encrypted;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::ForwardP;
use common::proto::client_rel::ForwardResultP;
use common::proto::client_rel::SRelayPacket;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use jni::JNIEnv;
use jni::objects::JByteArray;
use jni::objects::JString;
use jni::sys::jobject;
use jni_macro::jni;
use log::error;
use log::info;
use serde::Serialize;

use crate::JC;
use crate::RUNTIME;
use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::message::Message;
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
        }
    });
}

async fn send_message_inner(to: [u8; 32], content: String) -> anyhow::Result<()> {
    // 0. Save to local DB first (status = pending)
    let msg = Message::save_outgoing(to, &content)?;
    let msg_id = msg.inner.id;
    let msg_timestamp = msg.inner.timestamp;

    // 1. Look up contact and derive per-friendship shared key
    let contact = match Contact::get(&to) {
        Some(c) => c,
        None => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "recipient not in contacts".into() }.emit();
            return Err(anyhow!("recipient not in contacts"));
        },
    };

    let shared_key = match contact.shared_key() {
        Ok(k) => k,
        Err(e) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: e.to_string() }.emit();
            return Err(e);
        },
    };

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

    let fwd = ForwardP {
        to:      Bytes(to),
        from:    Bytes(our_ipk),
        payload: ByteVec(payload),
        sig:     Bytes(sig),
    };

    // 4. Send to relay
    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone()).ok_or_else(|| {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "not connected to relay".into() }.emit();
            anyhow!("not connected to relay")
        })?
    };

    let (mut send, mut recv) = conn.open_bi().await?;
    CRelayPacket::Forward(fwd).send(&mut send).await?;
    send.finish()?;

    // 5. Wait for result
    match SRelayPacket::unpack(&mut recv).await? {
        SRelayPacket::ForwardResult(ForwardResultP::Accepted) => {
            info!("MESSAGE: sent to {}", hex::encode(to));
            Message::mark_sent(&msg_id);
            MessageEv::Sent { id: msg_id, to, content, timestamp: msg_timestamp }.emit();
        },
        SRelayPacket::ForwardResult(ForwardResultP::NotFound) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "recipient not found".into() }.emit();
        },
        SRelayPacket::ForwardResult(ForwardResultP::InvalidSig) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: "invalid signature".into() }.emit();
        },
        SRelayPacket::ForwardResult(ForwardResultP::Error { reason }) => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason }.emit();
        },
        other => {
            Message::mark_failed(&msg_id);
            MessageEv::Failed { id: msg_id, to, reason: format!("unexpected: {other:?}") }.emit();
        },
    }

    Ok(())
}

/// Get paginated message history for a conversation.
/// Returns CBOR-encoded Vec<MessageRow>.
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getMessages(
    mut env: JNIEnv, _: JC, peer_ipk: JByteArray, limit: i32, before_id: JString,
) -> jobject {
    let peer: [u8; 32] = {
        let bytes = env.convert_byte_array(peer_ipk).unwrap();
        bytes.try_into().unwrap()
    };

    let before = if before_id.is_null() {
        String::new()
    } else {
        env.get_string(&before_id).map(|s| s.into()).unwrap_or_default()
    };

    let messages = Message::get_messages(&peer, limit.max(0) as u32, &before);

    let mut buf = vec![];
    ciborium::into_writer(&messages, &mut buf).unwrap();

    env.byte_array_from_slice(&buf).unwrap().into_raw()
}

/// Get all conversations (one entry per peer, latest message).
/// Returns CBOR-encoded Vec<MessageRow>.
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getConversations(env: JNIEnv, _: JC) -> jobject {
    let conversations = Message::get_conversations();

    let mut buf = vec![];
    ciborium::into_writer(&conversations, &mut buf).unwrap();

    env.byte_array_from_slice(&buf).unwrap().into_raw()
}

/// Public-safe contact info for Kotlin side.
#[derive(Serialize)]
struct ContactInfo {
    #[serde(with = "serde_bytes")]
    ipk:      [u8; 32],
    name:     String,
    added_at: u64,
}

/// Get all contacts.
/// Returns CBOR-encoded Vec<ContactInfo>.
#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn getContacts(env: JNIEnv, _: JC) -> jobject {
    let contacts: Vec<ContactInfo> = Contact::list()
        .into_iter()
        .map(|c| ContactInfo { ipk: c.ipk, name: c.name, added_at: c.added_at })
        .collect();

    let mut buf = vec![];
    ciborium::into_writer(&contacts, &mut buf).unwrap();

    env.byte_array_from_slice(&buf).unwrap().into_raw()
}
