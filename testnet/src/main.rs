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
mod deploy;
mod proc;
mod sandbox;

use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use crate::client::ClientProc;
use crate::sandbox::Sandbox;
use crate::sandbox::bin_path;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Subcommands: generate a real-server kit, or drive the scenario against
    // already-running remote relays. Default (no subcommand) = loopback sim.
    match args.get(1).map(String::as_str) {
        Some("deploy") => return deploy::run(&args[2..]),
        Some("remote") => return run_remote(&args[2..]).await,
        _ => {},
    }

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
    let relays: Vec<(SocketAddr, String)> =
        sb.relays.iter().map(|r| (r.addr, r.node_id.to_string())).collect();
    run_scenario(&client_bin, &sb.ca_path(), &relays).await
}

/// Drive the 1:1 scenario against the given relays (local sandbox or remote
/// servers): A homes on `relays[0]`, B on `relays[1]`. Each entry is the
/// relay's `(client/0 address, NodeId)`; `ca` signed all of them.
async fn run_scenario(client_bin: &Path, ca: &Path, relays: &[(SocketAddr, String)]) -> Result<()> {
    if relays.len() < 2 {
        bail!("the 1:1 scenario needs >=2 relays; got {}", relays.len());
    }
    let (a_addr, a_id) = &relays[0];
    let (b_addr, b_id) = &relays[1];
    let ca = ca.display().to_string();

    println!("\n— message scenario: client A @ {a_addr}  →  client B @ {b_addr} —");

    let mut a = ClientProc::spawn(
        "A",
        client_bin,
        &[
            ("E2E_LABEL", "A".into()),
            ("E2E_SEED", "1".into()),
            ("E2E_HOME_ADDR", a_addr.to_string()),
            ("E2E_HOME_ID", a_id.clone()),
            ("E2E_CA", ca.clone()),
        ],
    )
    .await?;
    let mut b = ClientProc::spawn(
        "B",
        client_bin,
        &[
            ("E2E_LABEL", "B".into()),
            ("E2E_SEED", "2".into()),
            ("E2E_HOME_ADDR", b_addr.to_string()),
            ("E2E_HOME_ID", b_id.clone()),
            ("E2E_CA", ca),
        ],
    )
    .await?;
    println!("✓ A connected  ipk={}…", short(&a.ipk));
    println!("✓ B connected  ipk={}…", short(&b.ipk));

    let n = b.cmd("publish_kp").await?;
    println!("✓ B published its KeyPackage to the DHT (count={})", n.trim());

    let gid = a.cmd(&format!("create_group {}", b.ipk)).await?;
    println!("✓ A fetched B's KP, built group {}…, published the Welcome", short(&gid));

    let activated = b.cmd("poll_welcomes").await?;
    if activated.trim() == "0" {
        bail!("B activated 0 Welcomes — the Welcome never reached B's home relay");
    }
    println!("✓ B fetched the Welcome and joined (activated={})", activated.trim());

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

/// `testnet remote <ca.pem> <relay_addr> <relay_id> [<relay_addr> <relay_id> …]`
/// — drive the 1:1 scenario against already-running relays (e.g. the real
/// servers), with `e2e-client` subprocesses on this host dialing them.
async fn run_remote(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() % 2 == 0 {
        bail!("usage: testnet remote <ca.pem> <relay_addr> <relay_id> <relay_addr> <relay_id> ...");
    }
    let ca = PathBuf::from(&args[0]);
    let mut relays: Vec<(SocketAddr, String)> = Vec::new();
    let mut i = 1;
    while i + 1 < args.len() {
        let addr: SocketAddr = args[i].parse().with_context(|| format!("relay addr `{}`", args[i]))?;
        relays.push((addr, args[i + 1].clone()));
        i += 2;
    }

    let client_bin = bin_path("e2e-client")?;
    println!("== promtuz testnet — remote scenario against {} relays ==", relays.len());
    match run_scenario(&client_bin, &ca, &relays).await {
        Ok(()) => {
            println!("\n🎉 remote e2e PASS — a 1:1 message crossed the real relays.");
            Ok(())
        },
        Err(e) => {
            eprintln!("\n✗ remote e2e FAILED: {e:#}");
            std::process::exit(1);
        },
    }
}

fn short(s: &str) -> &str {
    &s[..16.min(s.len())]
}
