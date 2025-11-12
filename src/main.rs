use anyhow::Result;
use common::msg::cbor::ToCbor;
use common::msg::resolver::RelayHello;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;
use tokio::io::AsyncWriteExt;
use tokio::join;

use crate::quic::dialer::connect_to_any_seed;
use crate::relay::Relay;
use crate::util::config::AppConfig;

mod quic;
mod relay;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);
    let relay = Relay::from_cfg(&cfg)?;

    use ProtoRole as PR;

    setup_crypto_provider()?;
    let server_cfg = {
        build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &[PR::Resolver, PR::Relay, PR::Peer, PR::Client],
        )?
    };

    let roots = load_root_ca(&cfg.network.root_ca_path)?;
    let mut endpoint = Endpoint::server(server_cfg, cfg.network.address)?;

    if let Ok(addr) = endpoint.local_addr() {
        println!("QUIC: listening at {:?}", addr);
    }

    let client_cfg = build_client_cfg(PR::Relay, &roots)?;
    endpoint.set_default_client_config(client_cfg);

    let conn = connect_to_any_seed(&endpoint, &cfg.resolver.seed, "arch.local").await?;

    let hello = RelayHello { relay_id: relay.id, version: relay.version };
    let hello = hello.to_cbor()?;

    let (mut tstream, mut rstream) = conn.open_bi().await?;

    println!("SENDING: RelayHello");
    tstream.write_all(&hello).await?;
    tstream.flush().await?;
    println!("SENT: RelayHello");

    let mut buf = vec![0u8; 4096];
    while let Ok(Some(n)) = rstream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        println!("RECV_PACKET: {:?}", &buf[..n]);
    }
    Ok(())
}
