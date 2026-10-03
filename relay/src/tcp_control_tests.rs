//! Production relays over loopback, with phones on the TLS carrier.

use std::time::Duration;

use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::Wake;
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;

use crate::quic::handler::client::events::forward::dispatch_to_deliver;
use crate::storage::MessageKey;
use crate::test_support::Client;
use crate::test_support::Node;
use crate::test_support::dispatch;
use crate::test_support::key;
use crate::test_support::queued;

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition never held");
}

async fn authenticated(node: &Node, identity: &SigningKey) -> Client {
    let client = Client::connect(node).await;
    assert!(client.authenticate(identity, identity, &client.connection).await);
    client
}

/// Reads only the ack: the stream ends once routing does, after the recipient's own ack.
async fn send(client: &Client, sent: DispatchP) -> DispatchAckP {
    let (mut send, mut receive) = client.connection.open_bi().await.unwrap();
    send.write_all(&CRelayPacket::Dispatch(sent).pack().unwrap()).await.unwrap();
    send.finish().unwrap();
    match SRelayPacket::unpack(&mut receive).await.unwrap() {
        SRelayPacket::DispatchAck(ack) => ack,
        other => panic!("expected a DispatchAck, got {other:?}"),
    }
}

async fn drain(client: &Client, first: Vec<CRelayPacket>) -> Vec<DeliverP> {
    let mut packets = first;
    packets.push(CRelayPacket::DrainQueue);
    client
        .request(packets)
        .await
        .into_iter()
        .map(|reply| match reply {
            SRelayPacket::Deliver(delivery) => delivery,
            other => panic!("expected only deliveries, got {other:?}"),
        })
        .collect()
}

/// Also a barrier: the relay answers the query only after the packets before it.
async fn ask_address(client: &Client, first: Vec<CRelayPacket>) {
    let mut packets = first;
    packets.push(CRelayPacket::Query(QueryP::PubAddress));
    assert!(matches!(client.request(packets).await.as_slice(), [SRelayPacket::QueryResult(_)]));
}

#[tokio::test]
async fn one_relay_authenticates_delivers_and_drains_over_tls_without_client_udp() {
    let node = Node::start(31, false).await;
    let messages = || node.relay.store.messages.len().unwrap();
    let (a_key, b_key) = (key(51), key(52));
    let b_ipk = b_key.verifying_key().to_bytes();

    let attacker = Client::connect(&node).await;
    assert!(!attacker.authenticate(&a_key, &b_key, &attacker.connection).await);
    let replayer = Client::connect(&node).await;
    let elsewhere = Client::connect(&node).await;
    assert!(!replayer.authenticate(&a_key, &a_key, &elsewhere.connection).await);
    assert!(node.relay.clients.read().is_empty(), "a bad proof never registers an IPK");
    drop((attacker, replayer, elsewhere));

    let a = authenticated(&node, &a_key).await;
    let b = authenticated(&node, &b_key).await;
    ask_address(&a, vec![]).await;
    ask_address(&b, vec![]).await;
    assert_eq!(node.relay.clients.read().len(), 2);

    let impersonated =
        dispatch(&b_key, a_key.verifying_key().to_bytes(), [1; 16], b"not my session");
    assert_eq!(send(&a, impersonated).await, DispatchAckP::InvalidSig);
    assert_eq!(messages(), 0);

    let live = dispatch(&a_key, b_ipk, [2; 16], b"opaque offer");
    assert!(matches!(send(&a, live.clone()).await, DispatchAckP::Queued { .. }));
    let (mut ack, mut incoming) = b.connection.accept_bi().await.unwrap();
    let SRelayPacket::Deliver(delivery) = SRelayPacket::unpack(&mut incoming).await.unwrap() else {
        panic!("missing live delivery")
    };
    assert_eq!(
        (&delivery.id, &delivery.from, &delivery.payload, &delivery.sig),
        (&live.id, &live.from, &live.payload, &live.sig)
    );
    assert!(delivery.accepted_at_ms > 0);
    ack.write_all(&CRelayPacket::DeliverAck.pack().unwrap()).await.unwrap();
    ack.finish().unwrap();
    eventually(|| messages() == 0).await;

    // TLS loss deregisters B, and the next dispatch waits in the durable queue of this sole relay.
    drop(b);
    eventually(|| !node.relay.clients.read().contains_key(&b_ipk)).await;
    let offline = dispatch(&a_key, b_ipk, [3; 16], b"opaque wake");
    assert!(matches!(send(&a, offline.clone()).await, DispatchAckP::Queued { .. }));
    assert_eq!(messages(), 1);

    let b = authenticated(&node, &b_key).await;
    let drained = drain(&b, vec![]).await;
    assert_eq!(
        drained.iter().map(|d| (d.id, d.payload.clone(), d.sig)).collect::<Vec<_>>(),
        [(offline.id, offline.payload, offline.sig)]
    );
    assert_eq!(messages(), 1, "a drain alone keeps custody");
    assert_eq!(drain(&b, vec![]).await, drained, "an unacknowledged drain is delivered again");
    ask_address(&b, vec![CRelayPacket::AckDrain]).await;
    assert_eq!(messages(), 0);
}

/// RLY-01: a write needs only two of three homes, so a drain at the third must fetch from them.
#[tokio::test]
async fn a_drain_at_the_third_home_delivers_what_only_the_other_two_hold() {
    let (a, b, c) =
        (Node::start(61, true).await, Node::start(62, true).await, Node::start(63, true).await);
    a.learn(&b);
    c.learn(&a);
    c.learn(&b);
    let bob = key(65);
    let bob_ipk = bob.verifying_key().to_bytes();
    let sent = dispatch(&key(64), bob_ipk, [7; 16], b"stored at two homes");
    let summary = crate::dht::forward::forward_to_homes(a.dht().clone(), sent.clone(), now_ms())
        .await
        .unwrap();
    assert_eq!(summary.stored_at.len(), 2);
    assert!(queued(c.dht(), &bob_ipk).is_empty());

    let phone = authenticated(&c, &bob).await;
    let timestamp = now_ms();
    let sig =
        bob.sign(&queue_fetch_signing_input(&bob_ipk, &c.dht().node_id, timestamp)).to_bytes();
    let drained = drain(&phone, vec![CRelayPacket::DrainAuth { timestamp, sig: sig.into() }]).await;
    assert_eq!(
        drained.iter().map(|d| d.id).collect::<Vec<_>>(),
        [sent.id],
        "one copy, delivered once"
    );
    for home in [&a, &b] {
        assert_eq!(
            queued(home.dht(), &bob_ipk),
            [[7; 16]],
            "homes keep custody until the user acks"
        );
    }
}

/// Rows older relays queued still drain: the delivered form `messages` held, with and without
/// `ttl_ms`, and a dispatch from before `ttl_ms`. A call offer past its life still goes out, so the
/// recipient can record the missed call; any other expired row does not.
#[tokio::test]
async fn rows_older_relays_queued_still_drain() {
    let node = Node::start(41, true).await;
    let bob = key(43);
    let bob_ipk = bob.verifying_key().to_bytes();
    let now = now_ms();
    let sent =
        |n: u8| DispatchP { accepted_at_ms: now, ..dispatch(&key(42), bob_ipk, [n; 16], &[n]) };
    let expired =
        |n, wake| DispatchP { accepted_at_ms: now - 60_000, wake, ttl_ms: 40_000, ..sent(n) };
    let delivered = |d: &DispatchP| (d.id, d.from, d.payload.clone(), d.sig, d.accepted_at_ms);
    let put = |ks: &fjall::Keyspace, row: &DispatchP, value: Vec<u8>| {
        let key = MessageKey::new(&bob_ipk, row.accepted_at_ms, &row.id.0);
        ks.insert(key.as_bytes(), value).unwrap();
    };
    let (messages, queue) = (&node.relay.store.messages, &node.relay.store.queue);

    let timed = DispatchP { ttl_ms: 30_000, ..sent(1) };
    let (id, from, payload, sig, at) = delivered(&timed);
    put(messages, &timed, (id, from, payload, sig, at, timed.ttl_ms).ser().unwrap());
    let untimed = sent(2);
    put(messages, &untimed, delivered(&untimed).ser().unwrap());
    let bool_wake = DispatchP { wake: Wake::Message, ..sent(3) };
    let (id, from, payload, sig, at) = delivered(&bool_wake);
    put(queue, &bool_wake, (bool_wake.to, from, id, payload, sig, at, true).ser().unwrap());
    let (missed, stale) = (expired(4, Wake::Call), expired(5, Wake::Message));
    put(queue, &missed, missed.ser().unwrap());
    put(queue, &stale, stale.ser().unwrap());

    let phone = authenticated(&node, &bob).await;
    let expected = [timed, untimed, missed, bool_wake].map(dispatch_to_deliver);
    assert_eq!(drain(&phone, vec![]).await, expected);
}
