//! Contact cards grant discovery, never pairing. The request key they carry is
//! no longer used, but the owner's signature covers it, so shared cards keep it.
use crate::data::identity::Identity;
use anyhow::{Result, anyhow, ensure};
use common::types::bytes::Bytes;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hpke_rs::{Hpke, HpkeKeyPair, Mode};
use hpke_rs_crypto::types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs_rust_crypto::HpkeRustCrypto;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const CARD_DOMAIN: &[u8] = b"promtuz-contact-card-v1";

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
#[derive(uniffi::Record)]
pub struct ContactCardPreview {
    pub ipk: Vec<u8>,
    pub name: String,
    pub already_contact: bool,
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
    })
}

/// The direct chat with a card's owner, named from the card until they tell us
/// more. Opening it sends nothing; their first message from us is the request.
#[uniffi::export]
pub fn chat_from_card(bytes: Vec<u8>) -> Result<Vec<u8>, crate::platform::CoreError> {
    let result = (|| -> Result<_> {
        let card = verify_card(&bytes)?;
        ensure!(Identity::get().is_none_or(|me| me.ipk() != card.ipk), "This is your own contact card");
        crate::data::peer_name::put(&card.ipk, &card.name)?;
        Ok(crate::data::conversation::Conversation::for_peer(&card.ipk)?.to_vec())
    })();
    result.map_err(Into::into)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn signer(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
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
}
