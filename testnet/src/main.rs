//! promtuz testnet — a sandboxed, all-loopback network harness.
//!
//! Spins up one resolver and `N` DHT-enabled relays as real binary
//! subprocesses (random ports, temp configs, CA-signed certs), then
//! drives simulated libcore clients through the full MLS stack. The first
//! true end-to-end validation of the network — no devices, no shared
//! state, every byte over real QUIC/TLS.
//!
//! Step 2 (this milestone): stand up the substrate and prove the relays
//! form a DHT over `peer/1`. Steps 3-4 add client subprocesses and assert
//! a 1:1 message crosses >=2 relays.

// WIP: several `Sandbox`/`RelayHandle` fields are consumed only by the
// not-yet-written client steps; silence the interim dead-code noise.
#![allow(dead_code)]

mod certs;
mod proc;
mod sandbox;

use std::time::Duration;

use anyhow::Result;

use crate::sandbox::Sandbox;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let relays = args.iter().skip(1).find_map(|a| a.parse::<usize>().ok()).unwrap_or(2);
    let keep = args.iter().any(|a| a == "--keep");

    println!("== promtuz testnet — {relays} relays (loopback, random ports) ==");

    let sb = match Sandbox::launch(relays, keep).await {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("\n✗ substrate failed to form: {e:#}");
            std::process::exit(1);
        },
    };

    println!(
        "\n✅ substrate healthy — resolver + {} relays formed a DHT over real QUIC/TLS",
        sb.relays.len()
    );

    // Brief hold so trailing logs surface, then tear everything down.
    tokio::time::sleep(Duration::from_secs(1)).await;
    sb.teardown().await;
    Ok(())
}
