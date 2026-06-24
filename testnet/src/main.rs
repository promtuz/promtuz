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
mod client;
mod proc;
mod sandbox;

use anyhow::Result;
use anyhow::bail;

use crate::client::ClientProc;
use crate::sandbox::Sandbox;
use crate::sandbox::bin_path;

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

    // Drive the 1:1 cross-relay MLS message scenario, then tear down.
    let scenario = run_message_scenario(&sb).await;
    sb.teardown().await;

    match scenario {
        Ok(()) => {
            println!(
                "\n🎉 e2e PASS — 1:1 MLS session across 2 relays: B's KeyPackage + A's Welcome \
                 each replicated relay-to-relay via the DHT, and B decrypted A's message."
            );
            Ok(())
        },
        Err(e) => {
            eprintln!("\n✗ e2e scenario FAILED: {e:#}");
            std::process::exit(1);
        },
    }
}

/// Two clients on *different* home relays exchange a 1:1 MLS message. B's
/// KeyPackage and A's Welcome each fan out across both relays via the DHT
/// (proving cross-relay replication); B then joins and decrypts A's
/// message. The application envelope itself is shuttled by the harness —
/// routing it through the relay `DispatchP` delivery path is a follow-up.
async fn run_message_scenario(sb: &Sandbox) -> Result<()> {
    let client_bin = bin_path("e2e-client")?;
    let ca = sb.ca_path();
    let r0 = &sb.relays[0];
    let r1 = &sb.relays[1];

    println!("\n— message scenario: client A @ relay-0  →  client B @ relay-1 —");

    let mut a = ClientProc::spawn(
        "A",
        &client_bin,
        &[
            ("E2E_LABEL", "A".into()),
            ("E2E_SEED", "1".into()),
            ("E2E_HOME_ADDR", r0.addr.to_string()),
            ("E2E_HOME_ID", r0.node_id.to_string()),
            ("E2E_CA", ca.display().to_string()),
        ],
    )
    .await?;
    let mut b = ClientProc::spawn(
        "B",
        &client_bin,
        &[
            ("E2E_LABEL", "B".into()),
            ("E2E_SEED", "2".into()),
            ("E2E_HOME_ADDR", r1.addr.to_string()),
            ("E2E_HOME_ID", r1.node_id.to_string()),
            ("E2E_CA", ca.display().to_string()),
        ],
    )
    .await?;
    println!("✓ A connected @ relay-0  ipk={}…", short(&a.ipk));
    println!("✓ B connected @ relay-1  ipk={}…", short(&b.ipk));

    let n = b.cmd("publish_kp").await?;
    println!("✓ B published its KeyPackage to the DHT (count={})", n.trim());

    let gid = a.cmd(&format!("create_group {}", b.ipk)).await?;
    println!("✓ A fetched B's KP, built group {}…, published the Welcome", short(&gid));

    let activated = b.cmd("poll_welcomes").await?;
    if activated.trim() == "0" {
        bail!("B activated 0 Welcomes — the Welcome never reached relay-1");
    }
    println!("✓ B fetched the Welcome from relay-1 and joined (activated={})", activated.trim());

    let plaintext = "hello across two relays";
    let env_hex = a.cmd(&format!("encrypt {} {}", b.ipk, hex::encode(plaintext))).await?;
    println!("✓ A encrypted \"{plaintext}\"");

    let pt_hex = b.cmd(&format!("decrypt {} {}", a.ipk, env_hex.trim())).await?;
    let got = String::from_utf8(hex::decode(pt_hex.trim())?)?;
    println!("✓ B decrypted: \"{got}\"");
    if got != plaintext {
        bail!("plaintext mismatch: sent {plaintext:?}, got {got:?}");
    }

    a.shutdown().await;
    b.shutdown().await;
    Ok(())
}

fn short(s: &str) -> &str {
    &s[..16.min(s.len())]
}
