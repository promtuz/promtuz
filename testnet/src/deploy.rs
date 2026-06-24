//! `testnet deploy` — generate a real-server deployment kit (root CA +
//! per-node certs + ready `config.toml` files), reusing the exact cert
//! logic the loopback harness uses.
//!
//! Output: `<out_dir>/{resolver,relay-0,relay-1,…}/`, each containing
//! `node.crt`, `node.key`, `ca.pem`, `config.toml` with **relative** cert
//! paths — so a node runs from its own dir (its RocksDB `./db` lands there
//! too). `scp` each dir to its box alongside the `relay`/`resolver` binary.
//!
//! Usage:
//! ```text
//!   testnet deploy <out_dir> <resolver_public_addr> <relay_bind_addr>...
//! ```
//! - `<resolver_public_addr>` — `ip:port` the RELAYS dial (seeded into their
//!   configs). The resolver itself binds `0.0.0.0:<that port>`.
//! - `<relay_bind_addr>` — `ip:port` each relay binds. Use the box's PUBLIC
//!   ip (not `0.0.0.0`) so the relay's outbound source address — which the
//!   resolver vends to the other relays — is deterministically dialable.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use crate::certs::Ca;
use crate::certs::Leaf;

pub fn run(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!(
            "usage: testnet deploy <out_dir> <resolver_public_addr> <relay_bind_addr>...\n  \
             e.g. testnet deploy ./deploy 10.0.0.1:40433 10.0.0.1:40434 10.0.0.2:40435"
        );
    }
    let out = PathBuf::from(&args[0]);
    let resolver_pub: SocketAddr =
        args[1].parse().with_context(|| format!("resolver_public_addr `{}`", args[1]))?;
    let relay_binds: Vec<SocketAddr> = args[2..]
        .iter()
        .map(|s| s.parse().with_context(|| format!("relay_bind_addr `{s}`")))
        .collect::<Result<_>>()?;
    if relay_binds.len() < 2 {
        bail!("need >=2 relays (K_MIN=2 quorum); got {}", relay_binds.len());
    }

    if out.exists() {
        fs::remove_dir_all(&out).ok();
    }
    fs::create_dir_all(&out).context("create out dir")?;

    let ca = Ca::new()?;
    // Persist the CA at the kit root so `add-relay` can issue more relays
    // under the same root later. `ca.key` stays local (never shipped to a
    // node); `ca.pem` is the trust anchor each node already gets.
    fs::write(out.join("ca.key"), ca.key_pem()).context("write ca.key")?;
    fs::write(out.join("ca.pem"), ca.cert_pem()).context("write ca.pem")?;

    // Resolver binds 0.0.0.0 on the public port; relays seed `resolver_pub`.
    let resolver_bind = SocketAddr::from(([0, 0, 0, 0], resolver_pub.port()));
    let resolver_leaf = ca.issue()?;
    write_node(&out, "resolver", &ca, &resolver_leaf, &resolver_config(resolver_bind))?;
    println!("resolver  bind {resolver_bind}   dialed at {resolver_pub}");

    for (i, bind) in relay_binds.iter().enumerate() {
        let leaf = ca.issue()?;
        let name = format!("relay-{i}");
        write_node(&out, &name, &ca, &leaf, &relay_config(*bind, &resolver_leaf.pubkey_hex, resolver_pub))?;
        println!("{name}   bind {bind}   id={}", leaf.node_id);
    }

    println!("\nkit written to {}", out.display());
    println!(
        "open these UDP ports in each box's firewall: resolver {} + the relay port(s) above",
        resolver_pub.port()
    );
    Ok(())
}

/// `testnet add-relay <kit_dir> <name> <relay_bind_addr> <resolver_public_addr>`
/// — issue ONE more relay under the kit's existing CA (no resolver), so it
/// joins the same network. Writes `<kit_dir>/<name>/`.
pub fn add_relay(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        bail!(
            "usage: testnet add-relay <kit_dir> <name> <relay_bind_addr> <resolver_public_addr>\n  \
             e.g. testnet add-relay ./deploy relay-canada 151.0.0.1:40436 187.0.0.1:40433"
        );
    }
    let kit = PathBuf::from(&args[0]);
    let name = &args[1];
    let bind: SocketAddr =
        args[2].parse().with_context(|| format!("relay_bind_addr `{}`", args[2]))?;
    let resolver_pub: SocketAddr =
        args[3].parse().with_context(|| format!("resolver_public_addr `{}`", args[3]))?;

    let ca_key = fs::read_to_string(kit.join("ca.key")).with_context(|| {
        format!(
            "read {}/ca.key — regenerate the kit with `testnet deploy` (older kits didn't persist the CA key)",
            kit.display()
        )
    })?;
    let ca_cert = fs::read_to_string(kit.join("ca.pem")).context("read ca.pem")?;
    let ca = Ca::load(ca_cert, &ca_key)?;

    // The resolver IPK seeded into the relay config — recovered from the
    // kit's resolver key (so the caller need only pass the resolver addr).
    let resolver_key = fs::read_to_string(kit.join("resolver/node.key"))
        .context("read resolver/node.key (for the resolver seed IPK)")?;
    let resolver_ipk_hex = crate::certs::pubkey_hex_from_key_pem(&resolver_key)?;

    let leaf = ca.issue()?;
    write_node(&kit, name, &ca, &leaf, &relay_config(bind, &resolver_ipk_hex, resolver_pub))?;
    println!("{name}  bind {bind}  id={}", leaf.node_id);
    println!("written to {}", kit.join(name).display());
    Ok(())
}

fn write_node(out: &Path, name: &str, ca: &Ca, leaf: &Leaf, config: &str) -> Result<()> {
    let dir = out.join(name);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("ca.pem"), ca.cert_pem())?;
    fs::write(dir.join("node.crt"), &leaf.cert_pem)?;
    fs::write(dir.join("node.key"), &leaf.key_pem)?;
    fs::write(dir.join("config.toml"), config)?;
    Ok(())
}

fn resolver_config(bind: SocketAddr) -> String {
    format!(
        "[network]\n\
         address = \"{bind}\"\n\
         cert_path = \"node.crt\"\n\
         key_path = \"node.key\"\n\
         root_ca_path = \"ca.pem\"\n"
    )
}

fn relay_config(bind: SocketAddr, resolver_ipk_hex: &str, resolver_pub: SocketAddr) -> String {
    format!(
        "[network]\n\
         address = \"{bind}\"\n\
         cert_path = \"node.crt\"\n\
         key_path = \"node.key\"\n\
         identity_key_path = \"node.key\"\n\
         root_ca_path = \"ca.pem\"\n\
         \n\
         [[resolver.seed]]\n\
         key = \"{resolver_ipk_hex}\"\n\
         addr = \"{resolver_pub}\"\n\
         \n\
         [dht]\n\
         enabled = true\n"
    )
}
