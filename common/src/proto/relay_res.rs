use std::fmt::Debug;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::proto::RelayId;
use crate::proto::pack::Packer;
use crate::sysutils::SystemLoad;
use crate::types::bytes::Bytes;

/// Each hello and heartbeat domain is distinct, so a signature never verifies as another kind.
pub const RELAY_HELLO_SIG_DOMAIN: &[u8] = b"promtuz-relay-hello-v1";

pub const RELAY_HEARTBEAT_SIG_DOMAIN: &[u8] = b"promtuz-relay-heartbeat-v1";

pub const GATEWAY_HELLO_SIG_DOMAIN: &[u8] = b"promtuz-gateway-hello-v1";

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum LifetimeP {
    /// `sig` covers [`relay_hello_signing_input`].
    RelayHello {
        relay_id:  RelayId,
        pubkey:    Bytes<32>,
        timestamp: u128,
        sig:       Bytes<64>,
    },

    HelloAck {
        resolver_time: u128,
    },

    /// `sig` covers [`relay_heartbeat_signing_input`].
    RelayHeartbeat {
        relay_id: RelayId,
        pubkey: Bytes<32>,
        timestamp: u128,
        sig: Bytes<64>,
        load: SystemLoad,
        uptime_seconds: u64,
    },

    /// Same transcript as `RelayHello` under `GATEWAY_HELLO_SIG_DOMAIN`. Appended last (postcard
    /// variant order).
    GatewayHello {
        gateway_id: RelayId,
        pubkey:     Bytes<32>,
        timestamp:  u128,
        sig:        Bytes<64>,
        /// The CA-issued leaf (DER) for `pubkey`, which must carry `PUSH_GATEWAY`.
        cert:       Vec<u8>,
    },
}

pub fn relay_hello_signing_input(
    relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128, binding: &[u8; 32],
) -> Vec<u8> {
    signing_input(RELAY_HELLO_SIG_DOMAIN, relay_id, pubkey, timestamp, binding)
}

pub fn relay_heartbeat_signing_input(
    relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128,
) -> Vec<u8> {
    signing_input(RELAY_HEARTBEAT_SIG_DOMAIN, relay_id, pubkey, timestamp, &[])
}

pub fn gateway_hello_signing_input(
    gateway_id: &RelayId, pubkey: &[u8; 32], timestamp: u128, binding: &[u8; 32],
) -> Vec<u8> {
    signing_input(GATEWAY_HELLO_SIG_DOMAIN, gateway_id, pubkey, timestamp, binding)
}

fn signing_input(
    domain: &[u8], relay_id: &RelayId, pubkey: &[u8; 32], timestamp: u128, binding: &[u8],
) -> Vec<u8> {
    [
        domain,
        &crate::PROTOCOL_VERSION.to_be_bytes(),
        relay_id.as_bytes(),
        pubkey,
        &timestamp.to_be_bytes(),
        binding,
    ]
    .concat()
}

/// Hello transcripts end with the session binding exported under this label, so a resolver
/// cannot forward a hello to another resolver and re-home the node there.
pub const NODE_HELLO_EXPORTER_LABEL: &[u8] = b"promtuz node hello v1";

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ResolverPacket {
    Lifetime(LifetimeP),
}

impl ResolverPacket {
    pub async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> anyhow::Result<()> {
        let packet = self.pack()?;
        tx.write_all(&packet).await?;
        Ok(tx.flush().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts() {
        let id = RelayId::from_bytes([1; 32]);
        crate::proto::golden(
            &[
                relay_hello_signing_input(&id, &[2; 32], 0x0102, &[3; 32]),
                relay_heartbeat_signing_input(&id, &[2; 32], 0x0102),
                gateway_hello_signing_input(&id, &[2; 32], 0x0102, &[3; 32]),
            ],
            "b161f2df1ff0ddb5d2550c17bdb3a901387c660c098ae94da92c9349d3ddf5b2
             2363b0d911f375b80ee040166a9b4dca5a6c587303a5d85de7cffa8f751fa904
             1e42714d7d2845563f2a86d608facd2f5e78939a0aa0fabe975f96e29fac234c",
        );
    }
}
