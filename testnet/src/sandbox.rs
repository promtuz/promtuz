//! Builds and supervises a throwaway promtuz network: one resolver and
//! `N` DHT-enabled relays, each a real binary subprocess with its own temp
//! working dir, generated config, and CA-signed cert.

use std::fs;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use common::quic::id::NodeId;

use crate::certs::Ca;
use crate::certs::Leaf;
use crate::proc::NodeProc;

pub struct RelayHandle {
    pub node: NodeProc,
    pub addr: SocketAddr,
    pub node_id: NodeId,
}

pub struct Sandbox {
    root: PathBuf,
    keep: bool,
    /// Kept past step 2: step 3 mints client leaf certs from it.
    ca: Ca,
    pub resolver: NodeProc,
    pub resolver_addr: SocketAddr,
    pub resolver_ipk_hex: String,
    pub relays: Vec<RelayHandle>,
}

impl Sandbox {
    pub async fn launch(num_relays: usize, keep: bool) -> Result<Self> {
        let num_relays = num_relays.max(2); // K_MIN=2 quorum needs >=2 homes
        let root = make_root()?;
        println!("sandbox root: {}", root.display());

        let ca = Ca::new()?;
        let ca_path = root.join("ca.pem");
        fs::write(&ca_path, ca.cert_pem()).context("write ca.pem")?;

        let resolver_bin = bin_path("resolver")?;
        let relay_bin = bin_path("relay")?;

        // ---- resolver ----
        let resolver_addr = local_addr(free_port()?);
        let resolver_dir = root.join("resolver");
        fs::create_dir_all(&resolver_dir)?;
        let resolver_leaf = ca.issue()?;
        write_certs(&resolver_dir, &resolver_leaf)?;
        fs::write(
            resolver_dir.join("config.toml"),
            resolver_config(resolver_addr, &resolver_dir, &ca_path),
        )?;
        let resolver =
            NodeProc::spawn("resolver", &resolver_bin, &["--config", "config.toml"], &resolver_dir, true)?;
        resolver
            .wait_for_log("listening at QUIC", Duration::from_secs(10))
            .await
            .context("resolver did not bind")?;
        println!("✓ resolver @ {resolver_addr}  ipk={}…", &resolver_leaf.pubkey_hex[..16]);

        // ---- relays ----
        let mut relays = Vec::with_capacity(num_relays);
        for i in 0..num_relays {
            let addr = local_addr(free_port()?);
            let dir = root.join(format!("relay-{i}"));
            fs::create_dir_all(&dir)?;
            let leaf = ca.issue()?;
            write_certs(&dir, &leaf)?;
            fs::write(
                dir.join("config.toml"),
                relay_config(addr, &dir, &ca_path, &resolver_leaf.pubkey_hex, resolver_addr),
            )?;
            let node =
                NodeProc::spawn(format!("relay-{i}"), &relay_bin, &["--config", "config.toml"], &dir, true)?;
            node.wait_for_log("listening at QUIC", Duration::from_secs(10))
                .await
                .with_context(|| format!("relay-{i} did not bind"))?;
            // Registration with the resolver proves this relay's cert +
            // identity over real QUIC/TLS. (The cold-start DHT bootstrap
            // races the resolver session and harmlessly fails once; the
            // scheduler retries on the next anti-entropy tick.)
            node.wait_for_log("resolver session started", Duration::from_secs(15))
                .await
                .with_context(|| format!("relay-{i} never registered with resolver"))?;
            println!("✓ relay-{i} @ {addr}  registered  id={}", leaf.node_id);
            relays.push(RelayHandle { node, addr, node_id: leaf.node_id });
        }

        // DHT convergence over peer/1. The cold-start bootstrap loses a race
        // with the resolver session, so relays converge on the first
        // anti-entropy tick (ANTI_ENTROPY_INTERVAL_MS = 30s): the
        // sparse-table retry re-queries the resolver and dials the peers it
        // returns. The last-spawned relay reaching a populated state proves
        // the peer/1 path (cert SAN == NodeId) end-to-end.
        println!("… waiting for DHT convergence over peer/1 (first anti-entropy tick ~30s)");
        let last = relays.last().context("no relays spawned")?;
        last.node
            .wait_for_any(&["bootstrap retry succeeded", "reached state"], Duration::from_secs(45))
            .await
            .context("relays did not converge into a DHT")?;
        println!("✓ DHT converged — peer/1 link established");

        Ok(Self {
            root,
            keep,
            ca,
            resolver,
            resolver_addr,
            resolver_ipk_hex: resolver_leaf.pubkey_hex,
            relays,
        })
    }

    /// Path to the sandbox root CA cert — clients trust this to verify
    /// the relay/0 server certs they connect to.
    pub fn ca_path(&self) -> PathBuf {
        self.root.join("ca.pem")
    }

    pub async fn teardown(mut self) {
        for r in &mut self.relays {
            r.node.kill().await;
        }
        self.resolver.kill().await;
        if self.keep {
            println!("kept sandbox at {}", self.root.display());
        } else {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn make_root() -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("promtuz-e2e-{}", std::process::id()));
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
    }
    fs::create_dir_all(&root).context("create sandbox root")?;
    Ok(root)
}

fn free_port() -> Result<u16> {
    // QUIC is UDP; bind :0, read the kernel-assigned port, release. There's
    // a small TOCTOU gap before the child rebinds — fine for loopback.
    let sock = UdpSocket::bind(("127.0.0.1", 0)).context("alloc port")?;
    Ok(sock.local_addr()?.port())
}

fn local_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Resolve a sibling binary of the running `testnet` exe (same
/// `target/<profile>/` dir).
pub(crate) fn bin_path(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().context("current_exe has no parent")?;
    let mut path = dir.join(name);
    if cfg!(windows) {
        path.set_extension("exe");
    }
    if !path.exists() {
        bail!(
            "binary `{name}` not found at {} — build the sandbox binaries first:\n    \
             cargo build -p relay -p resolver\n    \
             cargo build -p core --bin e2e-client --features e2e-client",
            path.display()
        );
    }
    Ok(path)
}

fn write_certs(dir: &Path, leaf: &Leaf) -> Result<()> {
    fs::write(dir.join("node.crt"), &leaf.cert_pem)?;
    fs::write(dir.join("node.key"), &leaf.key_pem)?;
    Ok(())
}

fn resolver_config(addr: SocketAddr, dir: &Path, ca: &Path) -> String {
    format!(
        "[network]\n\
         address = \"{addr}\"\n\
         cert_path = \"{crt}\"\n\
         key_path = \"{key}\"\n\
         root_ca_path = \"{ca}\"\n",
        crt = dir.join("node.crt").display(),
        key = dir.join("node.key").display(),
        ca = ca.display(),
    )
}

fn relay_config(
    addr: SocketAddr,
    dir: &Path,
    ca: &Path,
    resolver_ipk_hex: &str,
    resolver_addr: SocketAddr,
) -> String {
    let crt = dir.join("node.crt");
    let key = dir.join("node.key");
    format!(
        "[network]\n\
         address = \"{addr}\"\n\
         cert_path = \"{crt}\"\n\
         key_path = \"{key}\"\n\
         root_ca_path = \"{ca}\"\n\
         \n\
         [[resolver.seed]]\n\
         key = \"{resolver_ipk_hex}\"\n\
         addr = \"{resolver_addr}\"\n\
         \n\
         [dht]\n\
         enabled = true\n",
        crt = crt.display(),
        key = key.display(),
        ca = ca.display(),
    )
}
