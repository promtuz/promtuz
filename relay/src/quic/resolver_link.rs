//! Maintains connection with resolver

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::debug;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::quic::CloseReason;
use common::quic::id::NodeId;
use quinn::Connection;
use quinn::TransportConfig;
use tokio::sync::watch::Receiver;
use tokio::task::JoinHandle;

use crate::quic::dialer::connect_to_any_seed;
use crate::relay::RelayRef;
use crate::util::systime;

pub struct ResolverLink {
    relay: RelayRef,
    conn: Arc<Connection>,
    shutdown: Receiver<()>,
}

impl ResolverLink {
    /// Transport config for `Relay <-> Resolver`
    fn transport_cfg() -> Arc<TransportConfig> {
        let mut cfg = TransportConfig::default();

        cfg.keep_alive_interval(Some(Duration::from_secs(15)));

        Arc::new(cfg)
    }

    async fn id(&self) -> NodeId {
        self.relay.key.id()
    }

    /// Attaches with relay, it's job is to keep in contact with any resolver however possible.
    ///
    /// * `relay` - reference of relay to attach with
    /// * `rx` - shutdown receiver, used for closing the loop
    pub async fn attach(relay: RelayRef, rx: Receiver<()>) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let conn: Connection = {
                let mut cfg = (*relay.client_cfg).clone();
                cfg.transport_config(Self::transport_cfg());

                connect_to_any_seed(&relay.endpoint, &relay.cfg.resolver.seed, Some(cfg)).await?
            };

            let mut resolver = Self { relay, conn: Arc::new(conn), shutdown: rx };

            // Announcing Presence to Resolver
            resolver.hello().await?;

            let _ = resolver.handle().await;

            Ok(())
        })
    }

    pub async fn hello(&self) -> Result<()> {
        let mut send = self.conn.open_uni().await?;

        debug!("sending to resolver({})", self.conn.remote_address());

        ResolverPacket::Lifetime(LifetimeP::RelayHello {
            relay_id: self.id().await,
            timestamp: systime().as_millis(),
        })
        .send(&mut send)
        .await?;

        send.finish()?;

        Ok(())
    }

    pub async fn handle(&mut self) -> Result<()> {
        let conn = self.conn.clone();
        loop {
            let mut recv = tokio::select! {
                _ = self.shutdown.changed() => {
                    self.close();
                    break Ok(())
                },
                res = conn.accept_uni() => res?,
            };

            use LifetimeP::*;
            use ResolverPacket::*;
            match ResolverPacket::unpack(&mut recv).await? {
                Lifetime(HelloAck { resolver_time, .. }) => {
                    debug!(
                        "acknowledged by resolver({}) at {}",
                        self.conn.remote_address(),
                        resolver_time
                    );
                    continue;
                },
                packet => {
                    debug!("recv packet {:?}", packet);
                },
            }
        }
    }

    pub fn close(&self) {
        self.conn.close(CloseReason::ShuttingDown.code(), b"RelayShuttingDown");
    }
}
