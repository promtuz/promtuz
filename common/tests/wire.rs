//! A diff in wire.snap is a wire change that deployed peers will see.

use std::fmt::Debug;
use std::time::Duration;

use common::node::capability::NodeCapabilities;
use common::proto::client_rel::*;
use common::proto::client_res::*;
use common::proto::dht_p2p::*;
use common::proto::mls_wire::*;
use common::proto::pack::*;
use common::proto::push::*;
use common::proto::relay_res::*;
use common::proto::sticker::*;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use common::types::id::NodeId;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use expect_test::expect_file;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::Visitor;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;
use tokio::time::timeout;

const SIGNED_AT: u64 = 1_700_000_000_000;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn public(key: &SigningKey) -> Bytes<32> {
    Bytes(key.verifying_key().to_bytes())
}

fn node(key: &SigningKey) -> NodeId {
    NodeId::new(public(key))
}

fn sign(key: &SigningKey, transcript: &[u8]) -> Bytes<64> {
    Bytes(key.sign(transcript).to_bytes())
}

fn dispatch() -> DispatchP {
    DispatchP {
        to:             Bytes([1; 32]),
        from:           Bytes([2; 32]),
        id:             Bytes([3; 16]),
        payload:        ByteVec(vec![4]),
        sig:            Bytes([5; 64]),
        accepted_at_ms: 6,
        wake:           Wake::Message,
        ttl_ms:         7,
    }
}

fn token(len: usize) -> RegisterToken {
    RegisterToken::signed(&key(1), PushProvider::Fcm, vec![7; len])
}

async fn read_frames(mut input: &[u8]) -> String {
    let mut out = String::new();
    loop {
        let end = match unpack_optional::<ByteVec, _>(&mut input).await {
            Ok(Some(body)) => {
                out += &format!("{:?}, ", body.0);
                continue;
            },
            Ok(None) => "end".to_owned(),
            Err(UnpackError::ReadFailed(e)) => format!("ReadFailed({:?})", e.kind()),
            Err(e) => format!("{e:?}"),
        };
        return out + &end;
    }
}

#[tokio::test]
async fn frames_end_cleanly_only_between_frames() {
    let frame = ByteVec(vec![7; 3]).pack().unwrap();
    assert_eq!(frame, [0, 0, 0, 4, 3, 7, 7, 7]);
    for cut in 0..frame.len() {
        let want = if cut == 0 { "[7, 7, 7], end" } else { "[7, 7, 7], ReadFailed(UnexpectedEof)" };
        let input = [&frame[..], &frame[..cut]].concat();
        assert_eq!(read_frames(&input).await, want, "second frame cut at {cut}");
    }
    let cap = MAX_FRAME_BYTES as u32;
    let trailing = [&[0, 0, 0, 5][..], &frame[4..], &[9]].concat();
    for (label, input, want) in [
        ("a length at the cap", cap.to_be_bytes().to_vec(), "ReadFailed(UnexpectedEof)"),
        ("a length past the cap", (cap + 1).to_be_bytes().to_vec(), "FrameTooLarge(1048577)"),
        ("a byte after the value", trailing, "TrailingBytes"),
    ] {
        assert_eq!(read_frames(&input).await, want, "{label}");
    }
    let largest = ByteVec(vec![7; MAX_FRAME_BYTES - 3]);
    assert_eq!(unpack::<ByteVec, _>(&mut &largest.pack().unwrap()[..]).await.unwrap(), largest);
    let too_large = ByteVec(vec![7; MAX_FRAME_BYTES - 2]).pack();
    assert!(matches!(too_large, Err(PackError::FrameTooLarge(_))));
}

#[tokio::test(start_paused = true)]
async fn a_started_frame_times_out_but_an_idle_stream_waits() {
    let (_tx, mut rx) = tokio::io::duplex(64);
    let idle = timeout(Duration::from_secs(3600), unpack_optional::<ByteVec, _>(&mut rx)).await;
    assert!(idle.is_err(), "an idle stream gave {idle:?}");
    for sent in [&[0][..], &[0, 0, 0, 4, 3, 7]] {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(sent).await.unwrap();
        let started = Instant::now();
        let frame = unpack_optional::<ByteVec, _>(&mut rx).await;
        assert!(matches!(frame, Err(UnpackError::ReadTimedOut)), "{sent:?} gave {frame:?}");
        assert_eq!(started.elapsed(), FRAME_READ_TIMEOUT);
    }
}

fn decodes<T: Serialize + DeserializeOwned + Send>(value: T) -> bool {
    T::deser(&value.ser().unwrap()).is_ok()
}

#[test]
fn bounded_fields_accept_their_cap_and_refuse_one_more() {
    let descriptor = NodeDescriptor {
        id:     NodeId::from_bytes([1; 32]),
        addr:   "127.0.0.1:1".parse().unwrap(),
        pubkey: Bytes([1; 32]),
    };
    let approval = GroupMigrationApproval { who: Bytes([3; 32]), signature: Bytes([4; 64]) };
    let migration = |n| MlsEnvelopeP::GroupMigrationWelcome {
        group:     Bytes([1; 32]),
        branch:    Bytes([2; 32]),
        approvals: vec![approval.clone(); n],
        welcome:   WelcomeEnvelopeP {
            version:       1,
            group_id:      Bytes([1; 32]),
            sender_ipk:    Bytes([2; 32]),
            recipient_ipk: Bytes([3; 32]),
            welcome_blob:  ByteVec(vec![4]),
            kp_ref_used:   Bytes([5; 32]),
            sender_sig:    Bytes([6; 64]),
            pairing:       None,
        },
        history:   ByteVec(vec![]),
        signature: Bytes([5; 64]),
    };
    let manifest = |n| Manifest {
        pack_id:  [1; 16],
        store_id: 2,
        creator:  [3; 32],
        version:  4,
        name:     "p".into(),
        stickers: vec![ManifestSticker { id: [5; 32], width: 6, height: 7 }; n],
    };
    let envelope = |n| ManifestEnvelope::signed(&key(1), [1; 16], 2, 3, vec![4; n]);
    let blob = |n| StoreRequest::signed_blob(&key(1), [1; 16], 2, [3; 32], vec![4; n]);
    let wake =
        |n| WakeRequest { pseudonym: Bytes([1; 32]), payload: vec![2; n], class: Wake::Call };
    let receipt = ReceiptEntry { message_id: [1; 16], delivered_at: Some(2), read_at: None };
    let sharing = |n| AttachmentSharing {
        message_id: [1; 16],
        file_id:    [2; 32],
        size:       3,
        expires_at: 4,
        recipients: vec![[5; 32]; n],
    };
    let fields: [(&str, usize, &dyn Fn(usize) -> bool); 11] = [
        ("FindNodeResp.closer", MAX_FIND_NODE_RESULTS, &|n| {
            decodes(FindNodeResp { closer: vec![descriptor.clone(); n] })
        }),
        ("QueueFetchResp.messages", MAX_FETCH_QUEUE_BATCH, &|n| {
            decodes(QueueFetchResp { messages: vec![dispatch(); n], exhausted: true })
        }),
        ("GroupMigrationWelcome.approvals", 256, &|n| decodes(migration(n))),
        ("RegisterToken.token", MAX_PUSH_TOKEN_BYTES, &|n| decodes(token(n))),
        ("WakeRequest.payload", MAX_WAKE_PAYLOAD_BYTES, &|n| decodes(wake(n))),
        ("Manifest.stickers", PACK_MAX_STICKERS, &|n| decodes(manifest(n))),
        ("ManifestEnvelope.manifest_blob", MANIFEST_MAX_BYTES, &|n| decodes(envelope(n))),
        ("PutBlob.bytes", BLOB_MAX_BYTES, &|n| decodes(blob(n))),
        ("PutManifest.keys", PACK_MAX_STICKERS, &|n| {
            decodes(StoreRequest::PutManifest { env: envelope(1), keys: vec![Bytes([7; 32]); n] })
        }),
        ("ReceiptDetails.entries", 128, &|n| {
            decodes(ReceiptDetails { entries: vec![receipt.clone(); n] })
        }),
        ("AttachmentSharing.recipients", 255, &|n| decodes(sharing(n))),
    ];
    for (field, cap, decodes_with) in fields {
        assert!(decodes_with(cap), "{field} refused {cap}");
        assert!(!decodes_with(cap + 1), "{field} accepted {}", cap + 1);
    }
    assert!(!decodes(ReceiptDetails { entries: vec![] }), "ReceiptDetails.entries accepted none");
}

fn hello(relay: &SigningKey, binding: &[u8; 32]) -> DhtHello {
    let (node_id, pubkey) = (node(relay), public(relay));
    let sig = sign(relay, &dht_hello_signing_input(&node_id, &pubkey, SIGNED_AT, binding));
    DhtHello { node_id, pubkey, timestamp: SIGNED_AT, sig }
}

fn forward(relay: &SigningKey) -> Forward {
    let (dispatch, sender_relay_id) = (dispatch(), node(relay));
    let sig = sign(relay, &forward_signing_input(&dispatch.id, &sender_relay_id, SIGNED_AT));
    Forward { dispatch, sender_relay_id, timestamp: SIGNED_AT, sig }
}

fn lease(relay: NodeId, issued: u64, expires: u64) -> PresenceLease {
    let user = key(1);
    let transcript = presence_lease_signing_input(&public(&user), &relay, 1, issued, expires);
    PresenceLease {
        user:          public(&user),
        relay_id:      relay,
        version:       1,
        issued_at_ms:  issued,
        expires_at_ms: expires,
        user_sig:      sign(&user, &transcript),
    }
}

fn presence(relay: &SigningKey, lease: PresenceLease) -> RelayPresenceState {
    let mut state = RelayPresenceState {
        recipient: Bytes([9; 32]),
        who: lease.user,
        lease,
        state: PresenceState::Idle { since: SIGNED_AT },
        version: 1,
        observed_at_ms: SIGNED_AT,
        relay_pubkey: public(relay),
        relay_sig: Bytes([0; 64]),
    };
    state.relay_sig = sign(relay, &presence_state_signing_input(&state));
    state
}

fn put_blob(request: &mut StoreRequest) -> (&mut [u8; 32], &mut [u8; 64]) {
    let StoreRequest::PutBlob { key, sig, .. } = request else { unreachable!() };
    (key, &mut sig.0)
}

fn outcome<E: Debug>(verdict: Result<(), E>) -> String {
    verdict.map_or_else(|e| format!("{e:?}"), |()| "ok".to_owned())
}

fn accepted(ok: bool) -> String {
    String::from(if ok { "ok" } else { "rejected" })
}

type Row<P> = (&'static str, fn(&mut P), &'static str);

/// `window` is (late, stale verdict, early, future verdict): `valid` still verifies `late` ms after
/// it was signed and `early` ms before, and not one ms beyond either.
fn check<P: Clone>(
    name: &str, valid: P, verify: impl Fn(&P, u64) -> String,
    window: Option<(u64, &str, u64, &str)>, rows: &[Row<P>],
) {
    assert_eq!(verify(&valid, SIGNED_AT), "ok", "{name} as signed");
    if let Some((late, stale, early, future)) = window {
        for (label, at, want) in [
            ("at the late limit", SIGNED_AT + late, "ok"),
            ("past the late limit", SIGNED_AT + late + 1, stale),
            ("at the early limit", SIGNED_AT - early, "ok"),
            ("past the early limit", SIGNED_AT - early - 1, future),
        ] {
            assert_eq!(verify(&valid, at), want, "{name} verified {label}");
        }
    }
    for (label, tamper, want) in rows {
        let mut packet = valid.clone();
        tamper(&mut packet);
        assert_eq!(verify(&packet, SIGNED_AT), *want, "{name} {label}");
    }
}

#[test]
fn signed_packets_verify_only_untouched_and_inside_their_clock_window() {
    let (user, relay) = (key(1), key(2));
    let relay_key = relay.verifying_key().to_bytes();
    let skew = MAX_DHT_HELLO_SKEW_MS;
    let dht_window = Some((skew, "StaleTimestamp", skew, "FutureTimestamp"));
    let presence_skew = PRESENCE_STATE_MAX_SKEW_MS;
    let presence_window = Some((presence_skew, "rejected", presence_skew, "rejected"));

    check(
        "DhtHello",
        hello(&relay, &[5; 32]),
        |h, now| outcome(h.verify(now, &[5; 32])),
        Some((skew, "ClockSkew", skew, "ClockSkew")),
        &[
            ("with a flipped signature byte", |h| h.sig.0[0] ^= 1, "BadSignature"),
            ("naming an id its key does not hash to", |h| h.node_id = node(&key(3)), "IdMismatch"),
            ("signed on another session", |h| *h = hello(&key(2), &[6; 32]), "BadSignature"),
        ],
    );
    check(
        "Forward",
        forward(&relay),
        |f, now| outcome(f.verify(&relay_key, now)),
        dht_window,
        &[
            ("with a flipped signature byte", |f| f.sig.0[0] ^= 1, "BadForwardSig"),
            ("signed by another relay", |f| *f = forward(&key(3)), "BadForwardSig"),
        ],
    );
    let transcript = queue_fetch_signing_input(&public(&user), &node(&relay), SIGNED_AT);
    let fetch = QueueFetch {
        user_ipk:           public(&user),
        requester_relay_id: node(&relay),
        timestamp:          SIGNED_AT,
        user_sig:           sign(&user, &transcript),
    };
    check(
        "QueueFetch",
        fetch,
        |q, now| outcome(q.verify(now)),
        dht_window,
        &[
            ("with a flipped signature byte", |q| q.user_sig.0[0] ^= 1, "BadUserSig"),
            ("redirected to another relay", |q| q.requester_relay_id = node(&key(3)), "BadUserSig"),
        ],
    );
    let ids: Vec<[u8; 16]> = (0..MAX_FETCH_QUEUE_ACK_IDS).map(|i| [i as u8; 16]).collect();
    let transcript = queue_fetch_ack_signing_input(&public(&user), &node(&relay), &ids, SIGNED_AT);
    let ack = QueueFetchAck {
        user_ipk:           public(&user),
        requester_relay_id: node(&relay),
        delivered_ids:      ids,
        timestamp:          SIGNED_AT,
        user_sig:           sign(&user, &transcript),
    };
    check(
        "QueueFetchAck",
        ack,
        |a, now| outcome(a.verify(now)),
        dht_window,
        &[
            ("with a flipped signature byte", |a| a.user_sig.0[0] ^= 1, "BadUserSig"),
            ("redirected to another relay", |a| a.requester_relay_id = node(&key(3)), "BadUserSig"),
            ("acking one id more than a batch", |a| a.delivered_ids.push([0xff; 16]), "TooManyIds"),
        ],
    );
    let transcript = presence_consent_signing_input(&public(&user), &[9; 32], 1, SIGNED_AT, true);
    let consent = PresenceConsent {
        owner:        public(&user),
        recipient:    Bytes([9; 32]),
        version:      1,
        issued_at_ms: SIGNED_AT,
        granted:      true,
        user_sig:     sign(&user, &transcript),
    };
    check(
        "PresenceConsent",
        consent,
        |c, now| accepted(c.verify(now)),
        presence_window,
        &[
            ("with a flipped signature byte", |c| c.user_sig.0[0] ^= 1, "rejected"),
            ("redirected to another recipient", |c| c.recipient = Bytes([8; 32]), "rejected"),
        ],
    );
    const LIFE: u64 = PRESENCE_LEASE_MAX_MS;
    check(
        "PresenceLease",
        lease(node(&relay), SIGNED_AT, SIGNED_AT + LIFE),
        |l, now| accepted(l.verify(now)),
        Some((LIFE, "rejected", presence_skew, "rejected")),
        &[
            ("with a flipped signature byte", |l| l.user_sig.0[0] ^= 1, "rejected"),
            ("moved to another relay", |l| l.relay_id = node(&key(3)), "rejected"),
            (
                "one ms longer than a lease runs",
                |l| *l = lease(l.relay_id, SIGNED_AT, SIGNED_AT + LIFE + 1),
                "rejected",
            ),
        ],
    );
    check(
        "RelayPresenceState",
        presence(&relay, lease(node(&relay), SIGNED_AT - LIFE / 2, SIGNED_AT + LIFE / 2)),
        |s, now| accepted(s.verify(&node(&relay), now)),
        presence_window,
        &[
            ("with a flipped signature byte", |s| s.relay_sig.0[0] ^= 1, "rejected"),
            ("signed by another relay", |s| *s = presence(&key(3), s.lease.clone()), "rejected"),
        ],
    );
    check(
        "RegisterToken",
        token(MAX_PUSH_TOKEN_BYTES),
        |r, _| accepted(r.verify()),
        None,
        &[
            ("with a flipped signature byte", |r| r.sig.0[0] ^= 1, "rejected"),
            ("moved to another token", |r| r.token[0] ^= 1, "rejected"),
            (
                "signed over a token one byte past the cap",
                |r| *r = token(r.token.len() + 1),
                "rejected",
            ),
        ],
    );
    check(
        "ManifestEnvelope",
        ManifestEnvelope::signed(&user, [4; 16], 1, 2, vec![5; 8]),
        |e, _| accepted(e.verify()),
        None,
        &[
            ("with a flipped signature byte", |e| e.sig.0[0] ^= 1, "rejected"),
            ("given another version", |e| e.version += 1, "rejected"),
        ],
    );
    check(
        "PutBlob",
        StoreRequest::signed_blob(&user, [4; 16], 1, [6; 32], vec![5; 8]),
        |r, _| accepted(r.verify()),
        None,
        &[
            ("with a flipped signature byte", |r| put_blob(r).1[0] ^= 1, "rejected"),
            ("moved to another object key", |r| put_blob(r).0[0] ^= 1, "rejected"),
        ],
    );
}

/// Hex, with a run of four or more equal bytes written as `byte*count`.
fn hex(bytes: &[u8]) -> String {
    let mut out = vec![];
    for run in bytes.chunk_by(|a, b| a == b) {
        match run.len() {
            n if n >= 4 => out.push(format!("{:02x}*{n}", run[0])),
            _ => out.extend(run.iter().map(|b| format!("{b:02x}"))),
        }
    }
    out.join(" ")
}

fn encoded<T: Serialize>(value: &T) -> String {
    hex(&value.ser().unwrap())
}

/// A serde enum's variant names in declaration order, the order postcard numbers them in.
fn variants<T: DeserializeOwned>() -> &'static [&'static str] {
    struct Names(&'static [&'static str]);
    impl<'de> Deserializer<'de> for &mut Names {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("not an enum"))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self, _: &'static str, variants: &'static [&'static str], _: V,
        ) -> Result<V::Value, Self::Error> {
            self.0 = variants;
            Err(serde::de::Error::custom("names only"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier
            ignored_any
        }
    }
    let mut names = Names(&[]);
    let _ = T::deserialize(&mut names);
    names.0
}

macro_rules! ordinals {
    ($($ty:ty),* $(,)?) => {
        [$(format!("{}: {}", stringify!($ty), variants::<$ty>().join(" "))),*]
    };
}

#[test]
fn encodings_match_what_deployed_peers_expect() {
    use AppPayload as P;
    let (id, file, ipk) = ([0x11; 16], [0x33; 32], Bytes([0x22; 32]));
    let (s, addr) = (|| "hi".to_owned(), "127.0.0.1:1".parse().unwrap());
    let mut corpus = vec!["AppPayload".to_owned()];
    let mut names = vec![];
    for payload in [
        P::Text(s()),
        P::Receipt { kind: ReceiptKind::Delivered, upto: id },
        P::Edit { target: id, content: s() },
        P::Delete { target: id },
        P::React { target: id, emoji: s(), add: true },
        P::Reply { reply_to: id, content: s() },
        P::PairAck,
        P::P2p { candidates: vec![addr], relay: None, token: id, disco_key: file },
        P::Image {
            caption:  s(),
            group_id: None,
            mime:     s(),
            width:    1,
            height:   2,
            data:     vec![3],
        },
        P::Attachment {
            caption:  s(),
            group_id: Some(id),
            mime:     s(),
            name:     s(),
            size:     1,
            thumb:    vec![],
            file_id:  file,
        },
        P::FileWant { file_id: file },
        P::Post { reply_to: None, body: Body::Text(s()) },
        P::Revise { target: id, body: Body::Text(s()) },
        P::System(SystemEvent::Titled { title: s() }),
        P::Profile { name: s() },
        P::Avatar { revision: 1, avif: None },
        P::AvatarSync { known_revision: None, reply: true },
        P::AvatarAck { revision: 1 },
        P::P2pOffer {
            session:       id,
            in_reply_to:   None,
            expires_at_ms: 1,
            candidates:    vec![],
            relay:         None,
            token:         id,
            disco_key:     file,
        },
        P::Call(CallMsg::End { call: id, reason: CallEnd::Declined }),
        P::ProfileDetails { revision: 1, name: s(), bio: s(), card: vec![] },
        P::ProfileDetailsSync { known_revision: Some(1), reply: false },
        P::ProfileDetailsAck { revision: 1 },
        P::GroupPicture { revision: 1, avif: Some(vec![2]) },
        P::AttachmentSharing(AttachmentSharing {
            message_id: id,
            file_id:    file,
            size:       1,
            expires_at: 2,
            recipients: vec![ipk.0],
        }),
        P::ReceiptDetails(ReceiptDetails {
            entries: vec![ReceiptEntry {
                message_id:   id,
                delivered_at: Some(1),
                read_at:      None,
            }],
        }),
        P::Unpaired,
        P::GroupRequest(GroupRequest::Sync),
        P::GroupAdmins { admins: vec![ipk] },
        P::GroupWelcome { who: ipk, kp_ref: ipk, welcome: vec![1] },
        P::GroupInvitation { who: ipk, kp_ref: ipk, welcome: vec![1], history: vec![2] },
    ] {
        let bytes = payload.ser().unwrap();
        assert_eq!(P::deser(&bytes).unwrap(), payload);
        let (ordinal, _) = postcard::take_from_bytes::<u32>(&bytes).unwrap();
        let debug = format!("{payload:?}");
        let name = debug.split(|c: char| !c.is_alphanumeric()).next().unwrap();
        corpus.push(format!("{ordinal:>3} {name} {}", hex(&bytes)));
        names.push(name.to_owned());
    }
    assert_eq!(names, variants::<P>(), "one payload per variant, in declaration order");

    let node = NodeId::new([1; 32]);
    assert_eq!(node.ser().unwrap(), node.to_string().ser().unwrap());
    let sticker =
        StickerRef { pack: [0xaa; 16], id: [0xbb; 32], token: [0xcc; 32], store: 258 };
    let all = NodeCapabilities::all();
    let bits: Vec<_> =
        all.iter_names().map(|(name, bit)| format!("{name}={:#x}", bit.bits())).collect();
    corpus.extend([
        format!(
            "Wake No={} Message={} Call={}",
            encoded(&Wake::No),
            encoded(&Wake::Message),
            encoded(&Wake::Call)
        ),
        format!("StickerRef {}", encoded(&sticker)),
        format!("NodeId::new([1; 32]) {node}"),
        format!("blob_key {}", hex(&blob_key(&[1; 32], &[2; 32]))),
        format!("key_package_stash_prefix {}", hex(&key_package_stash_prefix(&[1; 32]))),
        format!("NodeCapabilities {} {}", hex(&all.encode()), bits.join(" ")),
    ]);
    corpus.extend(ordinals! {
        CHandshakePacket, SHandshakePacket, ServerHandshakeResultP, CRelayPacket, SRelayPacket,
        DispatchAckP, QueryP, QueryResultP, PresenceState, PresenceMode,
        MlsEnvelopeP, Body, CallMsg, CallCandidate, CallEnd, SystemEvent, GroupRequest, GroupChange,
        GroupMemberAction, ReceiptKind, KpPublishMode, KeyPackagePublishOutcome,
        KeyPackageFetchOutcome, KeyPackageRefillOutcome, WelcomePublishOutcome, WelcomeFetchOutcome,
        DhtPacket, DhtRequest, DhtResponse, ForwardOutcome,
        GatewayRequest, RegisterResponse, PushProvider, StoreRequest, StoreReject, StoreResponse,
        ResolverPacket, LifetimeP, ClientRequest, ClientResponse,
    });
    expect_file!["wire.snap"].assert_eq(&(corpus.join("\n") + "\n"));
}

#[test]
fn unknown_capability_bits_survive_decoding() {
    let bytes = [0x21, 0, 0, 0x80];
    let capabilities = NodeCapabilities::decode(&bytes).unwrap();
    assert_eq!(capabilities.bits(), 0x8000_0021);
    assert_eq!(capabilities.encode(), bytes);
}
