//! Welcome envelopes: sealing a Welcome for its recipient, and joining a group from one.

use common::crypto::verify_ed25519;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::welcome_envelope_signing_input;
use ed25519_dalek::Signer as DalekSigner;
use ed25519_dalek::SigningKey;
use openmls::prelude::MlsGroupJoinConfig;
use openmls::prelude::MlsMessageBodyIn;
use openmls::prelude::MlsMessageIn;
use openmls::prelude::MlsMessageOut;
use openmls::prelude::PURE_CIPHERTEXT_WIRE_FORMAT_POLICY;
use openmls::prelude::StagedWelcome;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::tls_codec::Serialize as _;

use super::MAX_GROUP_MEMBERS;
use super::group::MlsGroupHandle;
use super::provider::PromtuzMlsProvider;
use super::types::MlsGroupError;

type Result<T> = std::result::Result<T, MlsGroupError>;

pub fn make_welcome_envelope(
    welcome_msg: MlsMessageOut, group_id: [u8; 32], sender_ipk: [u8; 32],
    recipient_ipk: [u8; 32], kp_ref_used: [u8; 32], signer: &SigningKey,
) -> Result<WelcomeEnvelopeP> {
    seal_welcome_blob(
        encode_welcome(&welcome_msg)?,
        group_id,
        sender_ipk,
        recipient_ipk,
        kp_ref_used,
        signer,
    )
}

/// The whole `MlsMessageOut`, since openmls 0.8 only exposes `into_welcome` under `test-utils`.
pub fn encode_welcome(welcome_msg: &MlsMessageOut) -> Result<Vec<u8>> {
    welcome_msg.tls_serialize_detached().map_err(MlsGroupError::from_codec)
}

/// Seals an already-encoded Welcome as `sender_ipk`'s envelope, so a member can pass the founder's
/// Welcome to someone who knows them but not the founder.
pub fn seal_welcome_blob(
    welcome_blob: Vec<u8>, group_id: [u8; 32], sender_ipk: [u8; 32],
    recipient_ipk: [u8; 32], kp_ref_used: [u8; 32], signer: &SigningKey,
) -> Result<WelcomeEnvelopeP> {
    if welcome_blob.len() > MAX_WELCOME_BYTES {
        return Err(MlsGroupError::Internal(format!(
            "welcome_blob {} bytes exceeds MAX_WELCOME_BYTES = {}",
            welcome_blob.len(),
            MAX_WELCOME_BYTES
        )));
    }

    let transcript = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &group_id,
        &sender_ipk,
        &recipient_ipk,
        &kp_ref_used,
        &welcome_blob,
    );

    let sig = signer.sign(&transcript);

    Ok(WelcomeEnvelopeP {
        version: MLS_ENVELOPE_VERSION,
        group_id: group_id.into(),
        sender_ipk: sender_ipk.into(),
        recipient_ipk: recipient_ipk.into(),
        welcome_blob: welcome_blob.into(),
        kp_ref_used: kp_ref_used.into(),
        sender_sig: sig.to_bytes().into(),
        pairing: None,
    })
}

/// Verifies the envelope and joins the group. Whether to accept the sender at all is the caller's
/// decision.
pub fn process_welcome(
    provider: &PromtuzMlsProvider, envelope: &WelcomeEnvelopeP,
) -> Result<MlsGroupHandle> {
    let transcript = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &envelope.group_id.0,
        &envelope.sender_ipk.0,
        &envelope.recipient_ipk.0,
        &envelope.kp_ref_used.0,
        &envelope.welcome_blob.0,
    );
    verify_ed25519(&envelope.sender_ipk.0, &transcript, &envelope.sender_sig.0)
        .map_err(|_| MlsGroupError::BadSignature)?;

    let mls_msg = MlsMessageIn::tls_deserialize_exact(&envelope.welcome_blob.0)
        .map_err(MlsGroupError::from_codec)?;
    let welcome = match mls_msg.extract() {
        MlsMessageBodyIn::Welcome(w) => w,
        other => {
            return Err(MlsGroupError::Internal(format!(
                "welcome_blob does not carry a Welcome body (got {other:?})"
            )));
        },
    };

    let join_config = MlsGroupJoinConfig::builder()
        .use_ratchet_tree_extension(true)
        .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
        .padding_size(super::MLS_PADDING_SIZE)
        .build();
    provider.storage().atomic(|| {
        let staged = StagedWelcome::new_from_welcome(provider, &join_config, welcome, None)
            .map_err(MlsGroupError::from_openmls)?;

        // Everything below runs before `into_group`, so a refused Welcome writes no state.
        let roster = staged.members().count();
        if roster > MAX_GROUP_MEMBERS {
            return Err(MlsGroupError::Internal(format!(
                "Welcome roster is {roster} members, limit is {MAX_GROUP_MEMBERS}"
            )));
        }

        // The sender signs the envelope's group id, not the Welcome inside it. `home_for_group`
        // reads the group's real id while every other path trusts the envelope.
        let inner_gid = staged.group_context().group_id().as_slice();
        if inner_gid != envelope.group_id.0 {
            return Err(MlsGroupError::Internal(format!(
                "Welcome carries group {} but the envelope claims {}",
                hex::encode(inner_gid),
                hex::encode(envelope.group_id.0)
            )));
        }

        let context = staged.group_context().extensions();
        let is_group_chat = super::group::declares_group_meta(context);
        if is_group_chat && super::group::GroupMeta::from_extensions(context).is_none() {
            return Err(MlsGroupError::Internal("Welcome carries unsupported group rules".into()));
        }

        // Every leaf must be somebody, and the envelope's sender and recipient must
        // both hold a seat; a pair seats exactly those two.
        let recipient_ipk: [u8; 32] = envelope.recipient_ipk.0;
        let sender_ipk: [u8; 32] = envelope.sender_ipk.0;
        let mut saw_recipient = false;
        let mut saw_sender = false;
        for m in staged.members() {
            let Some(id) = super::credential::member_ipk(&m, is_group_chat) else {
                return Err(MlsGroupError::Internal(
                    "Welcome seats a leaf bound to no identity".into(),
                ));
            };
            saw_recipient |= id == recipient_ipk;
            saw_sender |= id == sender_ipk;
        }
        if !saw_recipient {
            return Err(MlsGroupError::Internal(
                "Welcome's inner credentials lack recipient_ipk identity (smuggling?)".into(),
            ));
        }
        if !saw_sender {
            return Err(MlsGroupError::Internal(
                "Welcome's inner credentials lack sender_ipk identity (smuggling?)".into(),
            ));
        }
        if !is_group_chat && (roster != 2 || sender_ipk == recipient_ipk) {
            return Err(MlsGroupError::Internal(
                "a pair Welcome must seat exactly the sender and the recipient".into(),
            ));
        }

        let mls_group = staged.into_group(provider).map_err(MlsGroupError::from_openmls)?;
        Ok(MlsGroupHandle::wrap(mls_group))
    })
}

#[cfg(test)]
mod tests {
    use openmls::prelude::BasicCredential;
    use openmls::prelude::Capabilities;
    use openmls::prelude::CredentialWithKey;
    use openmls::prelude::Extension;
    use openmls::prelude::Extensions;
    use openmls::prelude::GroupId;
    use openmls::prelude::KeyPackage;
    use openmls::prelude::MlsGroup;
    use openmls::prelude::MlsGroupCreateConfig;
    use openmls::prelude::RequiredCapabilitiesExtension;
    use openmls::prelude::UnknownExtension;

    use super::*;
    use crate::mls::GROUP_META_EXTENSION;
    use crate::mls::GroupMeta;
    use crate::mls::PROMTUZ_CIPHERSUITE;
    use crate::test_support::mls::*;

    /// `founder`, seated under `credential`, founds `gid` with `leaves` and returns the Welcome.
    fn welcome(
        founder: &Party, credential: CredentialWithKey, gid: [u8; 32], meta: Option<&GroupMeta>,
        leaves: &[KeyPackage],
    ) -> MlsMessageOut {
        let (provider, leaf) = (&founder.provider, &founder.leaf);
        let mut group = MlsGroupHandle::create(provider, leaf, credential, &gid, meta).unwrap();
        group.add_members(provider, leaf, leaves).unwrap().1
    }

    /// A Welcome for a group chat whose metadata this version cannot read.
    fn unreadable_rules(founder: &Party, gid: [u8; 32], leaves: &[KeyPackage]) -> MlsMessageOut {
        let extensions = Extensions::from_vec(vec![
            Extension::Unknown(u16::from(GROUP_META_EXTENSION), UnknownExtension(vec![0xFF; 4])),
            Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[GROUP_META_EXTENSION],
                &[],
                &[],
            )),
        ])
        .unwrap();
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(PROMTUZ_CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .with_group_context_extensions(extensions)
            .capabilities(Capabilities::new(
                None,
                Some(&[PROMTUZ_CIPHERSUITE]),
                Some(&[GROUP_META_EXTENSION]),
                None,
                None,
            ))
            .build();
        let (provider, leaf) = (&founder.provider, &founder.leaf);
        let id = GroupId::from_slice(&gid);
        let group = MlsGroup::new_with_group_id(provider, leaf, &config, id, founder.credential());
        let mut group = MlsGroupHandle::wrap(group.unwrap());
        group.add_members(provider, leaf, leaves).unwrap().1
    }

    /// Every refused Welcome leaves the recipient's database as it was: no group, no size, and the
    /// KeyPackage still there, since a refusal rolls back its use.
    #[test]
    fn a_refused_welcome_stores_nothing() {
        let (alice, bob, carol, dave) =
            (Party::new(1), Party::new(2), Party::new(3), Party::new(4));
        let kp = bob.kp();
        let room = GroupMeta::founded("room".into(), alice.ipk);
        let claiming = |ipk: [u8; 32], holder: &Party| CredentialWithKey {
            credential:    BasicCredential::new(ipk.to_vec()).into(),
            signature_key: holder.leaf.public().into(),
        };
        let pair = |gid| welcome(&alice, alice.credential(), gid, None, &[kp.clone()]);
        let sealed = |w, gid, from: &Party, to: [u8; 32]| {
            make_welcome_envelope(w, gid, from.ipk, to, kp_ref(&kp), &from.identity).unwrap()
        };
        let three = welcome(&alice, alice.credential(), [1; 32], None, &[kp.clone(), carol.kp()]);
        let impostor = welcome(&carol, claiming(dave.ipk, &carol), [2; 32], None, &[kp.clone()]);
        let forged = [kp.clone(), dave.key_package(claiming(carol.ipk, &dave))];
        let forged = welcome(&alice, alice.credential(), [3; 32], Some(&room), &forged);
        let daves = welcome(&dave, dave.credential(), [4; 32], None, &[kp.clone()]);
        let unreadable = unreadable_rules(&alice, [5; 32], &[kp.clone()]);
        let mut flipped = sealed(pair([8; 32]), [8; 32], &alice, bob.ipk);
        flipped.sender_sig.0[0] ^= 0xFF;
        let mut redirected = sealed(pair([9; 32]), [9; 32], &alice, bob.ipk);
        redirected.recipient_ipk = carol.ipk.into();
        // The case, the envelope, and whether its signature is what fails.
        let rows = [
            ("a pair seating a third person", sealed(three, [1; 32], &alice, bob.ipk), false),
            (
                "a pair's bare leaf claiming another",
                sealed(impostor, [2; 32], &carol, bob.ipk),
                false,
            ),
            ("a group leaf claiming another", sealed(forged, [3; 32], &alice, bob.ipk), false),
            ("a sender holding no seat", sealed(daves, [4; 32], &carol, bob.ipk), false),
            ("rules this version cannot read", sealed(unreadable, [5; 32], &alice, bob.ipk), false),
            (
                "a recipient holding no seat",
                sealed(pair([6; 32]), [6; 32], &alice, carol.ipk),
                false,
            ),
            (
                "an envelope naming another group",
                sealed(pair([7; 32]), [0; 32], &alice, bob.ipk),
                false,
            ),
            ("a flipped signature", flipped, true),
            ("a relay redirecting it", redirected, true),
        ];
        for (case, envelope, bad_signature) in rows {
            let before = dump(&bob.db);
            let refused = process_welcome(&bob.provider, &envelope).map(|_| ()).expect_err(case);
            assert_eq!(matches!(refused, MlsGroupError::BadSignature), bad_signature, "{case}");
            assert!(MlsGroupHandle::load(&bob.provider, &envelope.group_id.0).unwrap().is_none());
            assert_eq!(dump(&bob.db), before, "{case}");
        }
        let proper = sealed(pair([10; 32]), [10; 32], &alice, bob.ipk);
        assert!(process_welcome(&bob.provider, &proper).is_ok(), "the KeyPackage still joins");
    }

    /// GroupMeta is all that tells a group of two from a pair: the joiner reads it from the
    /// Welcome, and a pair carries none.
    #[test]
    fn group_meta_reaches_the_joiner_through_the_welcome() {
        let (alice, bob) = (Party::new(1), Party::new(2));
        let room = GroupMeta::founded("book club".into(), alice.ipk);
        for (gid, meta) in [([7; 32], Some(room)), ([8; 32], None)] {
            let kp = bob.kp();
            let w = welcome(&alice, alice.credential(), gid, meta.as_ref(), &[kp.clone()]);
            let joined =
                process_welcome(&bob.provider, &alice.envelope(&bob, gid, w, &kp)).unwrap();
            assert_eq!((joined.group_meta(), joined.member_count()), (meta, 2));
        }
    }
}
