//! Production relays over loopback, with phones on the TLS carrier.

use std::time::Duration;

use common::contracts::Support;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::Wake;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;

use crate::dht::rpc::rpc;
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
    let unstored = dispatch(&a_key, b_ipk, [4; 16], b"fails to store");
    for queued in [&offline, &unstored] {
        assert!(matches!(send(&a, queued.clone()).await, DispatchAckP::Queued { .. }));
    }
    assert_eq!(messages(), 2);

    let b = authenticated(&node, &b_key).await;
    let drained = drain(&b, vec![]).await;
    assert_eq!(
        drained.iter().map(|d| (d.id, d.payload.clone(), d.sig)).collect::<Vec<_>>(),
        [&offline, &unstored].map(|d| (d.id, d.payload.clone(), d.sig))
    );
    assert_eq!(messages(), 2, "a drain alone keeps custody");
    assert_eq!(drain(&b, vec![]).await, drained, "an unacknowledged drain is delivered again");
    ask_address(&b, vec![CRelayPacket::AckDrain { ids: vec![offline.id.0] }]).await;
    assert_eq!(messages(), 1, "what the phone did not store stays queued");
    assert_eq!(drain(&b, vec![]).await, drained[1..]);
    ask_address(&b, vec![CRelayPacket::AckDrain { ids: vec![unstored.id.0] }]).await;
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

/// A peer's services are probed once per connection, and a reconnect probes them again.
#[tokio::test]
async fn a_reconnect_probes_a_peers_services_again() {
    let (a, b) = (Node::start(71, true).await, Node::start(72, true).await);
    a.learn(&b);
    let peer = a.dht().routing.read().get(&b.dht().node_id).unwrap().descriptor();
    let inventory = DhtRequest::KeyPackageInventory { request: Vec::new().into() };
    let ask = || rpc(a.dht(), &peer, &inventory, 3_000);
    assert!(ask().await.is_some());

    let conn = a.dht().peer_conns.read()[&peer.id].0.clone();
    a.dht().peer_services.write().insert(conn.stable_id(), Support::default());
    assert!(ask().await.is_none(), "the next request trusts this connection's probe");

    conn.close(0u32.into(), b"reconnect");
    eventually(|| !a.dht().peer_services.read().contains_key(&conn.stable_id())).await;
    assert!(ask().await.is_some(), "a new connection is probed again");
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
    let put = |ks: &crate::storage::queue::Queue, row: &DispatchP, value: Vec<u8>| {
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

/// A backlog spans multiple key pages and the drain byte budget. Partial durable receipt,
/// disconnect and reconnect must retain every unacknowledged row, in order, with DHT disabled.
#[tokio::test]
async fn paged_backlog_survives_partial_ack_and_reconnect_without_dht() {
    let node = Node::start(71, false).await;
    let bob = key(72);
    let recipient = bob.verifying_key().to_bytes();
    let senders = [key(73), key(74)];
    let now = now_ms();
    let payload = vec![42; 16 * 1024];
    let mut ids = Vec::new();
    for n in 0..600u16 {
        let mut id = [0; 16];
        id[..2].copy_from_slice(&n.to_be_bytes());
        let mut row = dispatch(&senders[n as usize % 2], recipient, id, &payload);
        row.accepted_at_ms = now + u64::from(n);
        assert_eq!(
            node.relay.store.messages.admit(&row, row.accepted_at_ms).unwrap(),
            crate::storage::queue::QueueAdmission::Insert
        );
        ids.push(id);
    }
    node.relay.store.persist_barrier().wait().await.unwrap();
    let phone = authenticated(&node, &bob).await;
    let first = drain(&phone, vec![]).await;
    assert!(first.len() > 256 && first.len() < ids.len());
    assert_eq!(first.iter().map(|row| row.id.0).collect::<Vec<_>>(), ids[..first.len()]);
    let acknowledged = first.len() / 2;
    ask_address(&phone, vec![CRelayPacket::AckDrain { ids: ids[..acknowledged].to_vec() }]).await;
    drop(phone);
    eventually(|| !node.relay.clients.read().contains_key(&recipient)).await;
    let phone = authenticated(&node, &bob).await;
    let rest = drain(&phone, vec![]).await;
    assert_eq!(rest.iter().map(|row| row.id.0).collect::<Vec<_>>(), ids[acknowledged..]);
    ask_address(&phone, vec![CRelayPacket::AckDrain { ids: ids[acknowledged..].to_vec() }]).await;
    assert_eq!(node.relay.store.messages.len().unwrap(), 0);
    assert!(drain(&phone, vec![]).await.is_empty());
}

/// Disabling the mesh must preserve MLS custody through the actual client protocol:
/// signed publication, owner-only inventory, one-use fetches and reconnecting Welcome readers.
#[tokio::test]
async fn mls_custody_works_without_dht() {
    use crate::test_support::{kp_record, kp_sig, welcome};
    use common::contracts::services::{
        self,
        key_inventory::{Inventory, Request},
    };
    use common::proto::client_rel::ServerHandshakeResultP;
    use common::proto::mls_wire::*;

    tokio::time::timeout(Duration::from_secs(15), async {
        let node = Node::start(91, false).await;
        assert!(node.relay.dht.is_none());
        assert!(node.relay.mls.endpoint.is_none());
        assert!(node.relay.mls.peer_client_cfg.is_none());
        let (owner, sender) = (key(92), key(93));
        let owner_ipk = owner.verifying_key().to_bytes();
        let phone = Client::connect(&node).await;
        let Some(ServerHandshakeResultP::Accept { relay_node_id: Some(node_id), .. }) =
            phone.handshake(&owner, &owner, &phone.connection).await
        else { panic!("standalone relay must advertise its storage identity") };
        assert_eq!(node_id.0, *node.relay.node_id.as_bytes());
        let peer = authenticated(&node, &sender).await;
        let capabilities = phone.request(vec![CRelayPacket::ServiceCapabilities]).await;
        let [SRelayPacket::ServiceCapabilities { supported }] = capabilities.as_slice()
        else { panic!("missing capabilities") };
        let support = Support::decode(&supported.0).unwrap();
        assert!(support.supports(services::KEY_PACKAGE_CUSTODY, services::KEY_PACKAGE_CUSTODY_VERSION));
        assert!(support.supports(services::KEY_PACKAGE_INVENTORY, services::KEY_PACKAGE_INVENTORY_VERSION));

        let now = now_ms();
        let records = vec![kp_record(&owner, [1; 32], now + 3_600_000)];
        let publish = || CRelayPacket::PublishKeyPackage {
            sig: kp_sig(&owner, &records, now).into(), records: records.clone(), timestamp: now,
        };
        // A valid signature by another identity cannot replace this connection owner's stash.
        assert!(peer.request(vec![publish()]).await.is_empty());
        assert!(matches!(phone.request(vec![publish()]).await.as_slice(),
            [SRelayPacket::KeyPackagePublished { homes_succeeded: 1, quorum_met: true }]));
        let mut inventory = Request {
            owner: owner_ipk, delegate: node_id.0, timestamp: now, signature: [0; 64],
        };
        inventory.signature = owner.sign(&inventory.signing_input()).to_bytes();
        let inventory_packet = || CRelayPacket::KeyPackageInventory { request: inventory.encode().into() };
        assert!(peer.request(vec![inventory_packet()]).await.is_empty());
        let result = phone.request(vec![inventory_packet()]).await;
        let [SRelayPacket::KeyPackageInventory { inventory: encoded }] = result.as_slice()
        else { panic!("missing inventory") };
        let observed = Inventory::decode(&encoded.0).unwrap();
        assert!(observed.matches(&inventory) && observed.complete());
        assert_eq!(observed.homes.len(), 1);
        assert_eq!(observed.homes[0].node, node_id.0);
        assert_eq!(observed.homes[0].snapshot.as_ref().unwrap().references, [[1; 32]]);

        let fetch = || CRelayPacket::FetchKeyPackage {
            target_ipk: owner_ipk.into(), timestamp: now,
            sig: sender.sign(&kp_fetch_wrap_signing_input(
                MLS_WIRE_VERSION, &sender.verifying_key().to_bytes(), &owner_ipk, now,
            )).to_bytes().into(),
        };
        // Racing streams must never vend the same one-use package twice.
        let (first, second) = tokio::join!(
            peer.request(vec![fetch()]), peer.request(vec![fetch()]),
        );
        let replies: Vec<_> = first.into_iter().chain(second).collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies.iter().filter(|reply| matches!(reply,
            SRelayPacket::KeyPackageFetched { record: Some(_), .. })).count(), 1);
        assert_eq!(replies.iter().filter(|reply| matches!(reply,
            SRelayPacket::KeyPackageFetched { record: None, .. })).count(), 1);

        drop(phone);
        let phone = authenticated(&node, &owner).await;
        // A reconnect or a replayed publication must not resurrect consumed packages.
        assert!(matches!(phone.request(vec![publish()]).await.as_slice(),
            [SRelayPacket::KeyPackagePublished { quorum_met: true, .. }]));
        assert!(matches!(peer.request(vec![fetch()]).await.as_slice(),
            [SRelayPacket::KeyPackageFetched { record: None, .. }]));

        let envelope = welcome(&sender, owner_ipk, 7);
        let packet = CRelayPacket::PublishWelcome {
            sig: sender.sign(&welcome_publish_wrap_signing_input(
                MLS_WIRE_VERSION, &sender.verifying_key().to_bytes(), &envelope.welcome_blob.0, now,
            )).to_bytes().into(), envelope, timestamp: now,
        };
        assert!(matches!(peer.request(vec![packet]).await.as_slice(),
            [SRelayPacket::WelcomePublished { quorum_met: true }]));
        let fetch_welcome = || CRelayPacket::FetchWelcomes {
            timestamp: now,
            sig: owner.sign(&welcome_fetch_signing_input(
                MLS_WIRE_VERSION, &owner_ipk, &node.relay.node_id, now,
            )).to_bytes().into(),
        };
        assert!(peer.request(vec![fetch_welcome()]).await.is_empty());
        let result = phone.request(vec![fetch_welcome()]).await;
        let [SRelayPacket::WelcomesFetched { entries }] = result.as_slice()
        else { panic!("missing Welcomes") };
        assert_eq!(entries.len(), 1);
        let ids = vec![entries[0].welcome_id.0];
        drop(phone);
        let phone = authenticated(&node, &owner).await;
        assert_eq!(phone.request(vec![fetch_welcome()]).await, result,
            "unacknowledged Welcome must survive reconnect");
        let ack = || CRelayPacket::AckWelcomes {
            timestamp: now, welcome_ids: ids.iter().copied().map(Into::into).collect(),
            sig: owner.sign(&welcome_ack_signing_input(
                MLS_WIRE_VERSION, &owner_ipk, &node.relay.node_id, &ids, now,
            )).to_bytes().into(),
        };
        assert!(peer.request(vec![ack()]).await.is_empty());
        assert!(matches!(phone.request(vec![ack()]).await.as_slice(), [SRelayPacket::WelcomesAcked]));
        let result = phone.request(vec![fetch_welcome()]).await;
        assert!(matches!(result.as_slice(), [SRelayPacket::WelcomesFetched { entries }] if entries.is_empty()));
        assert!(node.relay.mls.peer_conns.read().is_empty());
        assert_eq!(node.relay.mls.routing.read().total_known(), 0);
    }).await.expect("standalone MLS custody timed out");
}
