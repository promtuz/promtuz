//! Contact cards grant discovery, never pairing. Requests use RFC 9180 HPKE
//! with a separate domain-derived key; the whole envelope is identity-signed.
use crate::{data::identity::Identity, db::messages::MESSAGES_DB};
use anyhow::{Result, anyhow, ensure};
use common::proto::{mls_wire::MlsEnvelopeP, pack::Packer};
use common::types::bytes::{ByteVec, Bytes};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hpke_rs::{Hpke, HpkeKeyPair, HpkePublicKey, Mode};
use hpke_rs_crypto::types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs_rust_crypto::HpkeRustCrypto;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const DOMAIN: &[u8] = b"promtuz-contact-request-v1";
const CARD_DOMAIN: &[u8] = b"promtuz-contact-card-v1";
pub(crate) static CONSENT: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static ACCEPT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Card {
    pub ipk: [u8; 32],
    pub name: String,
    pub request_key: [u8; 32],
    pub signature: Bytes<64>,
}
fn hpke() -> Hpke<HpkeRustCrypto> {
    Hpke::new(
        Mode::Base,
        KemAlgorithm::DhKem25519,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
    )
}
fn keys(signing: &SigningKey) -> Result<HpkeKeyPair> {
    let seed = Zeroizing::new(signing.to_bytes());
    let mut ikm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(None, seed.as_ref())
        .expand(b"promtuz-contact-request-key-v1", ikm.as_mut())
        .map_err(|_| anyhow!("key derivation"))?;
    hpke().derive_key_pair(ikm.as_ref()).map_err(|e| anyhow!("request key: {e:?}"))
}
fn card_input(ipk: &[u8; 32], name: &str, key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut bytes = CARD_DOMAIN.to_vec();
    bytes.extend(postcard::to_allocvec(&(ipk, name, key))?);
    Ok(bytes)
}
pub(crate) fn make_card(signing: &SigningKey, name: String) -> Result<Vec<u8>> {
    let ipk = signing.verifying_key().to_bytes();
    let request_key: [u8; 32] = keys(signing)?.public_key().as_slice().try_into()?;
    let signature = Bytes(signing.sign(&card_input(&ipk, &name, &request_key)?).to_bytes());
    Ok(postcard::to_allocvec(&Card { ipk, name, request_key, signature })?)
}
pub(crate) fn own_card() -> Result<Vec<u8>> {
    let me = Identity::get().ok_or_else(|| anyhow!("no identity"))?;
    make_card(&crate::data::identity::secret_key_signing(&me.ipk())?, me.name())
}
pub(crate) fn verify_card(bytes: &[u8]) -> Result<Card> {
    ensure!(bytes.len() <= 512, "card too large");
    let card: Card = postcard::from_bytes(bytes)?;
    ensure!(!card.name.trim().is_empty() && card.name.chars().count() <= 32, "invalid name");
    VerifyingKey::from_bytes(&card.ipk)?.verify_strict(
        &card_input(&card.ipk, &card.name, &card.request_key)?,
        &Signature::from_bytes(&card.signature.0),
    )?;
    Ok(card)
}
fn header(sender: &[u8; 32], recipient: &[u8; 32], id: &[u8; 16], expires: u64) -> Vec<u8> {
    [DOMAIN, sender, recipient, id, &expires.to_be_bytes()].concat()
}
fn seal(signing: &SigningKey, target: &Card, own: &[u8], now: u64) -> Result<MlsEnvelopeP> {
    let sender = signing.verifying_key().to_bytes();
    let id = common::crypto::get_nonce::<16>();
    let expires_ms = now.checked_add(LIFETIME_MS).ok_or_else(|| anyhow!("expiry overflow"))?;
    let aad = header(&sender, &target.ipk, &id, expires_ms);
    let (enc, ct) = hpke()
        .seal(&HpkePublicKey::new(target.request_key.to_vec()), DOMAIN, &aad, own, None, None, None)
        .map_err(|e| anyhow!("encrypt request: {e:?}"))?;
    let signature = signing.sign(&[aad, enc.clone(), ct.clone()].concat()).to_bytes();
    Ok(MlsEnvelopeP::ContactRequest {
        sender: Bytes(sender),
        recipient: Bytes(target.ipk),
        id: Bytes(id),
        expires_ms,
        encapsulated: ByteVec(enc),
        ciphertext: ByteVec(ct),
        signature: Bytes(signature),
    })
}
fn open(
    signing: &SigningKey, routed_sender: [u8; 32], envelope: &MlsEnvelopeP, now: u64,
) -> Result<(Vec<u8>, Card, u64)> {
    let MlsEnvelopeP::ContactRequest {
        sender,
        recipient,
        id,
        expires_ms,
        encapsulated,
        ciphertext,
        signature,
    } = envelope
    else {
        anyhow::bail!("not a request")
    };
    ensure!(
        sender.0 == routed_sender
            && recipient.0 == signing.verifying_key().to_bytes()
            && sender != recipient,
        "request identity mismatch"
    );
    ensure!(
        *expires_ms > now && *expires_ms <= now.saturating_add(LIFETIME_MS + 300_000),
        "request expired or future dated"
    );
    ensure!(ciphertext.0.len() <= 2048 && encapsulated.0.len() == 32, "request size");
    let aad = header(&sender.0, &recipient.0, &id.0, *expires_ms);
    VerifyingKey::from_bytes(&sender.0)?.verify_strict(
        &[aad.clone(), encapsulated.0.clone(), ciphertext.0.clone()].concat(),
        &Signature::from_bytes(&signature.0),
    )?;
    let plain = hpke()
        .open(
            &encapsulated.0,
            keys(signing)?.private_key(),
            DOMAIN,
            &aad,
            &ciphertext.0,
            None,
            None,
            None,
        )
        .map_err(|e| anyhow!("decrypt request: {e:?}"))?;
    let card = verify_card(&plain)?;
    ensure!(card.ipk == sender.0, "sender card mismatch");
    Ok((plain, card, *expires_ms))
}
pub(crate) fn receive(sender: [u8; 32], envelope: MlsEnvelopeP) -> Result<()> {
    let me = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
    let (bytes, card, expires) =
        open(&crate::data::identity::secret_key_signing(&me)?, sender, &envelope, now())?;
    if crate::data::contact::Contact::is_paired(&sender) {
        return Ok(());
    }
    store_request(&MESSAGES_DB.lock(), &card, &bytes, expires, false, now(), None)?;
    Ok(())
}
fn store_request(
    db: &rusqlite::Connection, card: &Card, bytes: &[u8], expires: u64, outgoing: bool, now: u64,
    wire: Option<&[u8]>,
) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute("DELETE FROM contact_requests WHERE expires_ms < ?1", [now])?;
    let count: u32 =
        tx.query_row("SELECT COUNT(*) FROM contact_requests WHERE outgoing=0", [], |r| r.get(0))?;
    if !outgoing && count >= 100 {
        return Ok(());
    }
    // Ignore newer requests from a declined peer until the current request expires.
    tx.execute("INSERT INTO contact_requests(peer,outgoing,name,card,expires_ms,wire) VALUES (?1,?2,?3,?4,?5,?6)
        ON CONFLICT(peer,outgoing) DO UPDATE SET name=excluded.name, card=excluded.card, expires_ms=excluded.expires_ms, wire=excluded.wire, status=0
        WHERE (contact_requests.status=0 OR excluded.outgoing=1) AND excluded.expires_ms > contact_requests.expires_ms",
        (card.ipk.as_slice(), outgoing, &card.name, bytes, expires, wire))?;
    tx.commit()?;
    Ok(())
}
fn now() -> u64 {
    crate::utils::systime().as_millis() as u64
}
pub(crate) fn outgoing_consent(peer: &[u8; 32]) -> Option<String> {
    MESSAGES_DB.lock().query_row("SELECT name FROM contact_requests WHERE peer=?1 AND outgoing=1 AND status=0 AND expires_ms>?2",
        (peer.as_slice(), now()), |r| r.get(0)).ok()
}

/// Requests own their small durable outbox so cancelling does not remove
/// unrelated message controls to the same person.
pub(crate) async fn retry_outgoing() {
    static RETRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(_one) = RETRY.try_lock() else { return };
    if crate::state::RELAY.read().as_ref().and_then(|r| r.connection.as_ref()).is_none() {
        return;
    }
    let Some(me) = Identity::get().map(|i| i.ipk()) else { return };
    let Ok(signer) = crate::data::identity::secret_key_signing(&me) else { return };
    let pending: Vec<([u8; 32], Vec<u8>)> = {
        let db = MESSAGES_DB.lock();
        let Ok(mut q) = db.prepare("SELECT peer,wire FROM contact_requests WHERE outgoing=1 AND status=0 AND expires_ms>?1 AND wire IS NOT NULL") else { return };
        q.query_map([now()], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };
    for (peer, bytes) in pending {
        if outgoing_consent(&peer).is_none() {
            continue;
        }
        let sent = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::messaging::dispatch_envelope(
                peer,
                me,
                &signer,
                bytes.clone(),
                common::proto::client_rel::Wake::Message,
                None,
            ),
        )
        .await;
        if matches!(sent, Ok(Ok(()))) {
            let _ = MESSAGES_DB.lock().execute(
                "UPDATE contact_requests SET wire=NULL WHERE peer=?1 AND outgoing=1 AND wire=?2",
                (peer.as_slice(), bytes),
            );
        }
    }
}

#[derive(uniffi::Record)]
pub struct ContactCardPreview {
    pub ipk: Vec<u8>,
    pub name: String,
    pub already_contact: bool,
    pub pending: bool,
}
#[uniffi::export]
pub fn contact_card(ipk: Vec<u8>) -> Result<Vec<u8>, crate::platform::CoreError> {
    let who = crate::api::messaging::to_ipk32(&ipk)?;
    if Identity::get().is_some_and(|i| i.ipk() == who) {
        return own_card().map_err(Into::into);
    }
    crate::data::peer_profile::get(&who)
        .map(|p| p.card)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| anyhow!("This contact hasn't shared a contact card yet").into())
}
#[uniffi::export]
pub fn preview_contact_card(
    bytes: Vec<u8>,
) -> Result<ContactCardPreview, crate::platform::CoreError> {
    let c = verify_card(&bytes)?;
    Ok(ContactCardPreview {
        ipk: c.ipk.to_vec(),
        name: c.name,
        already_contact: crate::data::contact::Contact::is_paired(&c.ipk),
        pending: outgoing_consent(&c.ipk).is_some(),
    })
}
#[uniffi::export(async_runtime = "tokio")]
pub async fn request_contact(bytes: Vec<u8>) -> Result<(), crate::platform::CoreError> {
    let result = crate::RUNTIME
        .spawn(async move {
            let _consent = CONSENT.lock();
            let target = verify_card(&bytes)?;
            let me = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
            ensure!(target.ipk != me, "This is your own contact card");
            if outgoing_consent(&target.ipk).is_some()
                || crate::data::contact::Contact::is_paired(&target.ipk)
            {
                return Ok(());
            }
            let signer = crate::data::identity::secret_key_signing(&me)?;
            let envelope = seal(&signer, &target, &own_card()?, now())?;
            let MlsEnvelopeP::ContactRequest { expires_ms, .. } = &envelope else { unreachable!() };
            let encoded = envelope.ser()?;
            // Consent and the sealed outgoing request are one transaction. A crash
            // cannot strand a "sent" request with no durable work left to deliver.
            store_request(
                &MESSAGES_DB.lock(),
                &target,
                &bytes,
                *expires_ms,
                true,
                now(),
                Some(&encoded),
            )?;
            crate::RUNTIME.spawn(retry_outgoing());
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(anyhow::Error::from)?;
    result.map_err(Into::into)
}
#[derive(uniffi::Record)]
pub struct ContactRequestRecord {
    pub ipk: Vec<u8>,
    pub name: String,
    pub outgoing: bool,
    pub expires_ms: u64,
}
#[uniffi::export]
pub fn contact_requests() -> Result<Vec<ContactRequestRecord>, crate::platform::CoreError> {
    let result = (|| -> Result<_> {
        let db = MESSAGES_DB.lock();
        let mut q = db.prepare("SELECT peer,name,outgoing,expires_ms FROM contact_requests WHERE status=0 AND expires_ms>?1 ORDER BY expires_ms DESC")?;
        Ok(q.query_map([now()], |r| {
            Ok(ContactRequestRecord {
                ipk: r.get(0)?,
                name: r.get(1)?,
                outgoing: r.get(2)?,
                expires_ms: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    })();
    result.map_err(Into::into)
}
#[uniffi::export(async_runtime = "tokio")]
pub async fn accept_contact_request(ipk: Vec<u8>) -> Result<(), crate::platform::CoreError> {
    let peer = crate::api::messaging::to_ipk32(&ipk)?;
    crate::RUNTIME.spawn(async move {
        let _single = ACCEPT.lock().await;
        let name: String = MESSAGES_DB.lock().query_row("SELECT name FROM contact_requests WHERE peer=?1 AND outgoing=0 AND status=0 AND expires_ms>?2",
            (peer.as_slice(), now()), |r| r.get(0))?;
        crate::messaging::pair_with_consent(peer, name, None).await?;
        MESSAGES_DB.lock().execute("UPDATE contact_requests SET status=2 WHERE peer=?1 AND outgoing=0", [peer.as_slice()])?;
        Ok::<_, anyhow::Error>(())
    }).await.map_err(anyhow::Error::from)??;
    Ok(())
}
#[uniffi::export]
pub fn dismiss_contact_request(
    ipk: Vec<u8>, outgoing: bool,
) -> Result<(), crate::platform::CoreError> {
    let peer = crate::api::messaging::to_ipk32(&ipk)?;
    let _accept =
        if outgoing {
            None
        } else {
            Some(ACCEPT.try_lock().map_err(|_| {
                anyhow!("A contact request is being accepted. Try again in a moment.")
            })?)
        };
    let _consent = CONSENT.lock();
    MESSAGES_DB.lock().execute("UPDATE contact_requests SET status=1,wire=NULL WHERE peer=?1 AND outgoing=?2 AND status=0", (peer.as_slice(), outgoing))
        .map_err(anyhow::Error::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signer(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    #[test]
    fn encrypted_preview_is_bound_to_both_identities_and_expires() {
        let alice = signer(1);
        let bob = signer(2);
        let mallory = signer(3);
        let alice_card = make_card(&alice, "Alice private name".into()).unwrap();
        let bob_card = verify_card(&make_card(&bob, "Bob".into()).unwrap()).unwrap();
        let request = seal(&alice, &bob_card, &alice_card, 1000).unwrap();
        let encoded = postcard::to_allocvec(&request).unwrap();
        assert!(!encoded.windows(b"Alice private name".len()).any(|w| w == b"Alice private name"));
        let (bytes, who, _) = open(&bob, alice.verifying_key().to_bytes(), &request, 1001).unwrap();
        assert_eq!(bytes, alice_card);
        assert_eq!(who.name, "Alice private name");
        assert!(open(&mallory, alice.verifying_key().to_bytes(), &request, 1001).is_err());
        assert!(open(&bob, mallory.verifying_key().to_bytes(), &request, 1001).is_err());
        assert!(
            open(&bob, alice.verifying_key().to_bytes(), &request, 1000 + LIFETIME_MS).is_err()
        );
        let mut tampered = request.clone();
        if let MlsEnvelopeP::ContactRequest { ciphertext, .. } = &mut tampered {
            ciphertext.0[0] ^= 1;
        }
        assert!(open(&bob, alice.verifying_key().to_bytes(), &tampered, 1001).is_err());
        let false_card = make_card(&mallory, "Alice private name".into()).unwrap();
        let impersonated = seal(&alice, &bob_card, &false_card, 1000).unwrap();
        assert!(open(&bob, alice.verifying_key().to_bytes(), &impersonated, 1001).is_err());
    }

    #[test]
    fn card_name_and_request_key_cannot_be_replaced_by_the_sharer() {
        let original = make_card(&signer(4), "Owner".into()).unwrap();
        let mut card = verify_card(&original).unwrap();
        card.name = "Impersonator".into();
        assert!(verify_card(&postcard::to_allocvec(&card).unwrap()).is_err());
        card = verify_card(&original).unwrap();
        card.request_key[0] ^= 1;
        assert!(verify_card(&postcard::to_allocvec(&card).unwrap()).is_err());
    }

    #[test]
    fn outgoing_consent_and_delivery_work_commit_together() {
        let db = crate::db::messages::open_in_memory();
        let bytes = make_card(&signer(6), "Peer".into()).unwrap();
        let card = verify_card(&bytes).unwrap();
        store_request(&db, &card, &bytes, 2000, true, 1000, Some(b"first")).unwrap();
        db.execute("UPDATE contact_requests SET status=1,wire=NULL", []).unwrap();
        // Explicit resend restores consent with fresh durable work.
        store_request(&db, &card, &bytes, 3000, true, 1001, Some(b"second")).unwrap();
        let read = || {
            db.query_row("SELECT status,expires_ms,wire FROM contact_requests", [], |r| {
                Ok((r.get::<_, u8>(0)?, r.get::<_, u64>(1)?, r.get::<_, Vec<u8>>(2)?))
            })
            .unwrap()
        };
        assert_eq!(read(), (0, 3000, b"second".to_vec()));
        db.execute_batch("CREATE TRIGGER fail_request BEFORE INSERT ON contact_requests BEGIN SELECT RAISE(ABORT, 'disk failure'); END;").unwrap();
        assert!(store_request(&db, &card, &bytes, 5000, true, 3001, Some(b"third")).is_err());
        // Pruning and insertion are atomic: failure cannot erase the previous work.
        assert_eq!(read(), (0, 3000, b"second".to_vec()));
    }

    #[test]
    fn replays_and_renewals_do_not_undo_a_decline_or_grant_consent() {
        let db = crate::db::messages::open_in_memory();
        let bytes = make_card(&signer(5), "Peer".into()).unwrap();
        let card = verify_card(&bytes).unwrap();
        store_request(&db, &card, &bytes, 2000, false, 1000, None).unwrap();
        db.execute("UPDATE contact_requests SET status=1", []).unwrap();
        store_request(&db, &card, &bytes, 3000, false, 1001, None).unwrap();
        let (status, expiry, outgoing): (u8, u64, bool) = db
            .query_row("SELECT status,expires_ms,outgoing FROM contact_requests", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((status, expiry, outgoing), (1, 2000, false));
        let contacts: u32 =
            db.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0)).unwrap();
        assert_eq!(contacts, 0);
        store_request(&db, &card, &bytes, 4000, false, 2001, None).unwrap();
        let status: u8 =
            db.query_row("SELECT status FROM contact_requests", [], |r| r.get(0)).unwrap();
        assert_eq!(status, 0);
    }
}
