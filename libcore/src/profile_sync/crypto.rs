//! Static HPKE reader keys protect current state. This is not MLS forward secrecy:
//! compromise of an identity secret exposes recorded grants encrypted to that identity.
use anyhow::{Result, anyhow, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use common::{
    crypto::{get_nonce, verify_ed25519},
    proto::profile::*,
    types::bytes::Bytes,
};
use ed25519_dalek::{Signer, SigningKey};
use hpke_rs::{HpkeKeyPair, HpkePublicKey};
use zeroize::Zeroizing;

const DOMAIN: &[u8] = b"promtuz-profile-wrap-v1";
pub(super) fn keys(signing: &SigningKey) -> Result<HpkeKeyPair> {
    crate::contact_card::derive_keys(signing, b"promtuz-profile-reader-key-v1")
}
pub(super) fn reader_key(signing: &SigningKey) -> Result<ReaderKey> {
    let mut key = ReaderKey {
        owner: signing.verifying_key().to_bytes().into(),
        key: keys(signing)?.public_key().as_slice().try_into().map(Bytes)?,
        signature: [0; 64].into(),
    };
    key.signature = signing.sign(&key.input()).to_bytes().into();
    Ok(key)
}
pub(super) fn seal(
    signing: &SigningKey, field: Field, object: [u8; 32], version: u64, plain: &[u8],
    readers: &[ReaderKey],
) -> Result<Publication> {
    let key = Zeroizing::new(get_nonce::<32>());
    let mut value = Value {
        owner: signing.verifying_key().to_bytes().into(),
        object: object.into(),
        field,
        version,
        ciphertext: vec![].into(),
        signature: [0; 64].into(),
    };
    let nonce = get_nonce::<24>();
    let ct = XChaCha20Poly1305::new((&*key).into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: &value.context() })
        .map_err(|_| anyhow!("profile encryption failed"))?;
    value.ciphertext = [nonce.as_slice(), &ct].concat().into();
    value.signature = signing.sign(&value.input()).to_bytes().into();
    let digest = blake3::hash(&value.input());
    let mut grants = Vec::with_capacity(readers.len());
    for reader in readers {
        verify_ed25519(&reader.owner.0, &reader.input(), &reader.signature.0)?;
        let aad = [DOMAIN, digest.as_bytes(), reader.owner.0.as_slice()].concat();
        let wrapped = crate::contact_card::hpke().seal(
            &HpkePublicKey::new(reader.key.0.to_vec()),
            DOMAIN,
            &aad,
            key.as_ref(),
            None,
            None,
            None,
        );
        let (enc, ct) = match wrapped {
            Ok(wrapped) => wrapped,
            // A contact can sign an invalid X25519 key. Exclude that reader instead of
            // preventing publication or withdrawal for every other contact.
            Err(e) => {
                log::warn!("PROFILE: unusable reader encryption key: {e:?}");
                continue;
            },
        };
        grants.push(Grant {
            reader: reader.owner,
            encapsulated: enc.as_slice().try_into().map(Bytes)?,
            wrapped_key: ct.as_slice().try_into().map(Bytes)?,
        });
    }
    Ok(Publication { value, grants })
}
pub(super) fn verify(value: &Value, owner: &[u8; 32], field: Field) -> Result<()> {
    ensure!(
        value.owner.0 == *owner
            && value.field == field
            && value.version > 0
            && value.version <= i64::MAX as u64
            && value.ciphertext.len() <= MAX_CIPHERTEXT,
        "invalid profile value"
    );
    verify_ed25519(owner, &value.input(), &value.signature.0)?;
    Ok(())
}
pub(super) fn open(signing: &SigningKey, value: &Value, grant: &Grant) -> Result<Vec<u8>> {
    ensure!(grant.reader.0 == signing.verifying_key().to_bytes(), "wrong profile reader");
    verify(value, &value.owner.0, value.field)?;
    let digest = blake3::hash(&value.input());
    let aad = [DOMAIN, digest.as_bytes(), grant.reader.0.as_slice()].concat();
    let key = Zeroizing::new(
        crate::contact_card::hpke()
            .open(
                &grant.encapsulated.0,
                keys(signing)?.private_key(),
                DOMAIN,
                &aad,
                &grant.wrapped_key.0,
                None,
                None,
                None,
            )
            .map_err(|e| anyhow!("profile grant: {e:?}"))?,
    );
    ensure!(key.len() == 32 && value.ciphertext.len() >= 40, "invalid profile ciphertext");
    let (nonce, ct) = value.ciphertext.split_at(24);
    XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| anyhow!("invalid profile key"))?
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &value.context() })
        .map_err(|_| anyhow!("profile decryption failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audience_rotation_authentication_and_field_binding() {
        let owner = SigningKey::from_bytes(&[1; 32]);
        let alice = SigningKey::from_bytes(&[2; 32]);
        let bob = SigningKey::from_bytes(&[3; 32]);
        let mut readers = vec![reader_key(&alice).unwrap(), reader_key(&bob).unwrap()];
        readers.sort_by_key(|r| r.owner.0);
        let old = seal(&owner, Field::Name, [4; 32], 1, b"old", &readers).unwrap();
        let grant = |p: &Publication, k: &SigningKey| {
            p.grants.iter().find(|g| g.reader.0 == k.verifying_key().to_bytes()).unwrap().clone()
        };
        assert_eq!(open(&bob, &old.value, &grant(&old, &bob)).unwrap(), b"old");
        let new =
            seal(&owner, Field::Name, [4; 32], 2, b"new", &[reader_key(&alice).unwrap()]).unwrap();
        assert_eq!(open(&alice, &new.value, &grant(&new, &alice)).unwrap(), b"new");
        assert!(
            open(&bob, &new.value, &grant(&old, &bob)).is_err(),
            "removed reader's old grant cannot decrypt new state"
        );
        assert!(open(&bob, &new.value, &grant(&new, &alice)).is_err());
        let mut changed = new.value.clone();
        changed.field = Field::Bio;
        assert!(open(&alice, &changed, &grant(&new, &alice)).is_err());
        let mut changed = new.value.clone();
        changed.ciphertext[25] ^= 1;
        assert!(open(&alice, &changed, &grant(&new, &alice)).is_err());
        let mut forged = reader_key(&alice).unwrap();
        forged.key = reader_key(&bob).unwrap().key;
        assert!(seal(&owner, Field::Name, [4; 32], 3, b"secret", &[forged]).is_err());
        assert!(verify(&new.value, &bob.verifying_key().to_bytes(), Field::Name).is_err());
        let mut unusable = reader_key(&bob).unwrap();
        unusable.key = [0; 32].into();
        unusable.signature = bob.sign(&unusable.input()).to_bytes().into();
        let publication = seal(
            &owner,
            Field::Name,
            [4; 32],
            3,
            b"still shared",
            &[reader_key(&alice).unwrap(), unusable],
        )
        .unwrap();
        assert_eq!(publication.grants.len(), 1);
        assert_eq!(
            open(&alice, &publication.value, &publication.grants[0]).unwrap(),
            b"still shared"
        );
    }
}
