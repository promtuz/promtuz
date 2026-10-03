//! The relay's one-shot RPCs over its resolver session.

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::GatewayDescriptor;
use common::proto::client_res::RelayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::server::resolver_link::Session;
use quinn::Connection;

/// The registered resolver session for one-shot RPCs; empty while the link reconnects.
#[derive(Clone, Debug)]
pub struct ResolverLinkHandle(pub Session);

impl ResolverLinkHandle {
    /// Returns once a session is registered, or once the link has stopped.
    pub async fn ready(&self) {
        let _ = self.0.clone().wait_for(Option::is_some).await;
    }

    fn current_connection(&self) -> Option<Connection> {
        self.0.borrow().clone()
    }

    pub async fn get_bootstrap_peers(
        &self, near: [u8; 32], count_xor_near: u8, count_rtt_near: u8,
    ) -> Result<(Vec<RelayDescriptor>, Vec<RelayDescriptor>)> {
        let conn = self
            .current_connection()
            .context("no live resolver session for GetBootstrapPeers")?;

        let req = ClientRequest::GetBootstrapPeers { near, count_xor_near, count_rtt_near };
        let bytes = req.pack()?;

        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&bytes).await?;
        send.finish()?;

        let resp = ClientResponse::unpack(&mut recv).await?;
        match resp {
            ClientResponse::GetBootstrapPeers { xor_near, rtt_near } => Ok((xor_near, rtt_near)),
            other => Err(anyhow!(
                "GetBootstrapPeers: resolver returned unexpected variant {:?}",
                other
            )),
        }
    }

    pub async fn get_gateways(&self) -> Result<Vec<GatewayDescriptor>> {
        let conn =
            self.current_connection().context("no live resolver session for GetGateways")?;

        let bytes = ClientRequest::GetGateways().pack()?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&bytes).await?;
        send.finish()?;

        match ClientResponse::unpack(&mut recv).await? {
            ClientResponse::GetGateways { gateways } => Ok(gateways),
            other => Err(anyhow!("GetGateways: resolver returned unexpected variant {:?}", other)),
        }
    }
}
