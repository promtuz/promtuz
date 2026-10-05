use crate::quic::handler::client::ClientCtxHandle;
use anyhow::Result;
use common::proto::{
    Sender,
    client_rel::SRelayPacket,
    profile::{Request, Response},
};

pub(super) async fn handle(
    request: Request, ctx: ClientCtxHandle, tx: &mut quinn::SendStream,
) -> Result<()> {
    let write = matches!(request, Request::Register(_) | Request::Publish(_));
    let allowed =
        if write { ctx.limits.profile_write.check() } else { ctx.limits.profile_read.check() };
    if allowed.is_err() {
        SRelayPacket::Profile(Response::Rejected).send(tx).await?;
        return Ok(());
    }
    let owner = ctx.ipk.to_bytes();
    let store = ctx.relay.store.clone();
    let notify = if let Request::Publish(p) = &request {
        let store = store.clone();
        let field = p.value.field;
        let mut readers =
            tokio::task::spawn_blocking(move || store.profiles.readers(&owner, field)).await??;
        readers.extend(p.grants.iter().map(|g| g.reader.0));
        readers.sort_unstable();
        readers.dedup();
        readers
    } else {
        vec![]
    };
    let reply = tokio::task::spawn_blocking(move || -> Result<Response> {
        let accepted = match request {
            Request::Register(key) => store.profiles.register(&owner, &key)?,
            Request::Publish(value) => store.profiles.publish(&owner, &value)?,
            Request::ReaderKey { owner } => {
                return Ok(Response::ReaderKey(store.profiles.reader_key(&owner.0)?));
            },
            Request::Head { field } => return store.profiles.head(&owner, field),
            Request::Fetch { owner: target, field, known_version } => {
                return store.profiles.fetch(&owner, &target.0, field, known_version);
            },
        };
        Ok(if accepted { Response::Accepted } else { Response::Rejected })
    })
    .await??;
    if write && reply == Response::Accepted {
        ctx.relay.store.persist_barrier().wait().await?;
    }
    if reply == Response::Accepted && !notify.is_empty() {
        let connections: Vec<_> = {
            let clients = ctx.relay.clients.read();
            notify.into_iter().filter_map(|reader| clients.get(&reader).cloned()).collect()
        };
        super::spawn_tied(&ctx.cancel, async move {
            let tasks = connections
                .into_iter()
                .map(|connection| async move {
                    let _ = tokio::time::timeout(super::STREAM_OPEN_TIMEOUT, async {
                        let (mut tx, _) = connection.open_bi().await?;
                        SRelayPacket::ProfileChanged { owner: owner.into() }.send(&mut tx).await?;
                        tx.finish()?;
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                })
                .collect();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                super::bounded_fanout(tasks, 8),
            )
            .await;
        });
    }
    SRelayPacket::Profile(reply).send(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::profiles::tests::fixture;
    use crate::test_support::{Client, Node, key};
    use common::proto::{client_rel::CRelayPacket, pack::Unpacker, profile::Field};

    async fn request(client: &Client, request: Request) -> Response {
        let mut replies = client.request(vec![CRelayPacket::Profile(request)]).await;
        assert_eq!(replies.len(), 1);
        let SRelayPacket::Profile(reply) = replies.remove(0) else { panic!("profile response") };
        reply
    }
    #[tokio::test]
    async fn local_profiles_replace_without_queueing_and_notify_revoked_readers() {
        let node = Node::start(90, false).await;
        let owner = Client::connect(&node).await;
        let viewer = Client::connect(&node).await;
        let outsider = Client::connect(&node).await;
        let (a, b, c) = (key(31), key(41), key(51));
        assert!(owner.authenticate(&a, &a, &owner.connection).await);
        assert!(viewer.authenticate(&b, &b, &viewer.connection).await);
        assert!(outsider.authenticate(&c, &c, &outsider.connection).await);
        let who = a.verifying_key().to_bytes();
        let (reader, published) = fixture(1, vec![b.verifying_key().to_bytes()]);
        assert_eq!(request(&owner, Request::Register(reader)).await, Response::Accepted);
        assert_eq!(request(&owner, Request::Publish(published.clone())).await, Response::Accepted);
        let (_tx, mut rx) =
            tokio::time::timeout(std::time::Duration::from_secs(5), viewer.connection.accept_bi())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            SRelayPacket::unpack(&mut rx).await.unwrap(),
            SRelayPacket::ProfileChanged { owner: who.into() }
        );
        assert_eq!(
            request(
                &viewer,
                Request::Fetch { owner: who.into(), field: Field::Avatar, known_version: None }
            )
            .await,
            Response::Value { value: published.value.clone(), grant: published.grants[0].clone() }
        );
        assert_eq!(
            request(
                &viewer,
                Request::Fetch { owner: who.into(), field: Field::Avatar, known_version: Some(1) }
            )
            .await,
            Response::Unchanged
        );
        assert!(matches!(
            request(
                &outsider,
                Request::Fetch { owner: who.into(), field: Field::Avatar, known_version: None }
            )
            .await,
            Response::Withdrawn { .. }
        ));
        let (_, revoked) = fixture(2, vec![]);
        assert_eq!(request(&owner, Request::Publish(revoked.clone())).await, Response::Accepted);
        assert_eq!(request(&owner, Request::Publish(published)).await, Response::Rejected);
        let (_tx, mut rx) =
            tokio::time::timeout(std::time::Duration::from_secs(5), viewer.connection.accept_bi())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            SRelayPacket::unpack(&mut rx).await.unwrap(),
            SRelayPacket::ProfileChanged { .. }
        ));
        assert_eq!(
            request(
                &viewer,
                Request::Fetch { owner: who.into(), field: Field::Avatar, known_version: Some(1) }
            )
            .await,
            Response::Withdrawn { value: revoked.value }
        );
        assert_eq!(node.relay.store.messages.len().unwrap(), 0);
        assert_eq!(node.relay.store.queue.len().unwrap(), 0);
    }
}
