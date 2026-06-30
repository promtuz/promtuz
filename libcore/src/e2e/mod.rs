//! Headless end-to-end client driver (feature `e2e-client`).
//!
//! One process = one simulated libcore client, driven by the `testnet`
//! orchestrator over a dead-simple line protocol on stdin/stdout. It
//! bypasses libcore's global runtime entirely — no JNI `KeyManager`, no
//! global `Identity`/`Contact`/`RELAY`/`ENDPOINT` — using an explicit
//! deterministic identity key and an in-memory MLS store. What it does
//! drive is the *real* code: the MLS pipeline (`lazy_create_group`,
//! `build_application_envelope_bytes`, …) and a real
//! [`RelayDhtClient`] (via its explicit-signer seam) over a live
//! authenticated `client/0` connection to a real relay subprocess.
//!
//! Drives the same `MlsContext` client shape as the integration harness,
//! over `RelayDhtClient`/`client/0` instead of the removed `peer/1` path.
//!
//! ## Control protocol (one command per stdin line; reply per stdout line)
//!
//! ```text
//!   info                      -> ok info <ipk_hex>
//!   publish_kp                -> ok publish_kp <count>
//!   create_group <to_hex>     -> ok create_group <group_id_hex>
//!   poll_welcomes             -> ok poll_welcomes <activated_count>
//!   encrypt <to_hex> <txt_hex>-> ok encrypt <envelope_hex>
//!   decrypt <from_hex> <env_hex> -> ok decrypt <plaintext_hex>
//!   quit                      -> ok quit   (then exits)
//! ```
//! Failures reply `err <cmd> <message>`. On startup the client emits
//! `ready <ipk_hex>` once connected. Binary fields are hex; logs go to
//! stderr so they never pollute the protocol on stdout.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::str::SplitWhitespace;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::PROTOCOL_VERSION;
use common::proto::Sender;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::SHandshakePacket as SHSP;
use common::proto::client_rel::ServerHandshakeResultP as SHSRP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::pack::Unpacker;
use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use quinn::Connection;
use quinn::Endpoint;
use tokio::io::AsyncBufReadExt;

use crate::messaging::InboundDecoded;
use crate::messaging::MlsContext;
use crate::messaging::build_application_envelope_bytes;
use crate::messaging::lazy_create_group;
use crate::messaging::leaf_signer_for_group;
use crate::messaging::process_application_inbound_for;
use crate::messaging::process_welcome_inbound_no_contacts;
use crate::db::mls::apply_mls_migrations;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::KpOutcomeFilter;
use crate::quic::relay_dht_client::RelayDhtClient;

/// Entry point for the `e2e-client` binary. Builds a runtime and runs the
/// control loop; any fatal error exits non-zero.
pub fn run() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(run_async()) {
        eprintln!("e2e-client fatal: {e:#}");
        std::process::exit(1);
    }
}

async fn run_async() -> Result<()> {
    let label = std::env::var("E2E_LABEL").unwrap_or_else(|_| "client".into());
    let seed: u8 = env_var("E2E_SEED")?.parse().map_err(|_| anyhow!("E2E_SEED must be a u8"))?;
    let home_addr: SocketAddr =
        env_var("E2E_HOME_ADDR")?.parse().map_err(|e| anyhow!("E2E_HOME_ADDR: {e}"))?;
    let home_id = env_var("E2E_HOME_ID")?;
    let ca = PathBuf::from(env_var("E2E_CA")?);

    let mut client = Client::connect(seed, home_addr, &home_id, &ca).await?;
    eprintln!("[{label}] connected to {home_addr}; ipk={}", hex::encode(client.ipk));
    println!("ready {}", hex::encode(client.ipk));

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut args = line.split_whitespace();
        let cmd = args.next().unwrap_or("");
        match client.handle(cmd, &mut args).await {
            Ok(Some(data)) => println!("ok {cmd} {data}"),
            Ok(None) => {
                println!("ok {cmd}");
                if cmd == "quit" {
                    break;
                }
            },
            Err(e) => println!("err {cmd} {}", format!("{e:#}").replace('\n', " ")),
        }
    }
    Ok(())
}

/// One simulated client: explicit identity + in-memory MLS state + a real
/// [`RelayDhtClient`] over a live connection to its home relay.
struct Client {
    ipk: [u8; 32],
    ipk_signer: SigningKey,
    provider: PromtuzMlsProvider,
    stash: KeyPackageStash,
    buffer: EpochCatchupBuffer,
    dht: RelayDhtClient,
    /// recipient IPK -> founder group handle (sender side; the receiver
    /// joins via the provider so it needs no handle here).
    groups: HashMap<[u8; 32], MlsGroupHandle>,
}

impl Client {
    async fn connect(seed: u8, home_addr: SocketAddr, home_id: &str, ca: &Path) -> Result<Self> {
        let ipk_signer = SigningKey::from_bytes(&[seed; 32]);
        let ipk = ipk_signer.verifying_key().to_bytes();

        // Fresh in-memory MLS state — this driver never touches libcore's
        // global SQLite or the JNI keystore.
        let conn = {
            let mut c = rusqlite::Connection::open_in_memory()?;
            apply_mls_migrations(&mut c);
            Arc::new(Mutex::new(c))
        };
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let buffer = EpochCatchupBuffer::new(conn);

        let (qconn, home_node_id) = connect_home(home_addr, home_id, ca, ipk, &ipk_signer).await?;
        let dht = RelayDhtClient::new_with_signer(qconn, ipk, home_node_id, ipk_signer.clone());

        Ok(Self { ipk, ipk_signer, provider, stash, buffer, dht, groups: HashMap::new() })
    }

    fn ctx(&self) -> MlsContext<'_, RelayDhtClient> {
        MlsContext {
            provider: &self.provider,
            stash:    &self.stash,
            buffer:   &self.buffer,
            dht:      &self.dht,
        }
    }

    async fn handle(
        &mut self, cmd: &str, args: &mut SplitWhitespace<'_>,
    ) -> Result<Option<String>> {
        Ok(match cmd {
            "info" => Some(hex::encode(self.ipk)),
            "publish_kp" => Some(self.publish_kp().await?.to_string()),
            "create_group" => Some(self.create_group(next_ipk(args)?).await?),
            "poll_welcomes" => Some(self.poll_welcomes().await?.to_string()),
            "encrypt" => {
                let to = next_ipk(args)?;
                let text = hex::decode(args.next().unwrap_or_default())?;
                Some(hex::encode(self.encrypt(to, &text)?))
            },
            "decrypt" => {
                let from = next_ipk(args)?;
                let env = hex::decode(args.next().unwrap_or_default())?;
                Some(hex::encode(self.decrypt(from, &env)?))
            },
            "quit" => None,
            other => bail!("unknown command: {other}"),
        })
    }

    /// Mint a fresh KP stash and publish one record to the K homes via the
    /// real `RelayDhtClient` (Tier-1 wrapper -> home fan-out).
    async fn publish_kp(&self) -> Result<usize> {
        let kps = self.stash.ensure_stash_full(&self.provider, &self.ipk_signer)?;
        let to_publish = if kps.is_empty() { &kps[..] } else { &kps[..1] };
        self.dht
            .publish_keypackages(to_publish, KpOutcomeFilter::Default)
            .await
            .map_err(|e| anyhow!("publish kp: {e}"))?;
        Ok(to_publish.len())
    }

    /// Founder side: fetch `to`'s KeyPackage from the DHT, build the 1:1
    /// group, and publish the Welcome to the K homes. Returns the group id.
    async fn create_group(&mut self, to: [u8; 32]) -> Result<String> {
        let group = {
            let ctx = self.ctx();
            lazy_create_group(&ctx, &self.ipk, &self.ipk_signer, &to).await?
        };
        let gid = hex::encode(group.group_id());
        self.groups.insert(to, group);
        Ok(gid)
    }

    /// Joiner side: fetch + process any pending Welcomes from our home,
    /// then ack them so the homes GC. Returns how many activated a group.
    async fn poll_welcomes(&self) -> Result<usize> {
        let entries =
            self.dht.fetch_welcomes().await.map_err(|e| anyhow!("fetch welcomes: {e}"))?;
        let mut processed: Vec<[u8; 8]> = Vec::with_capacity(entries.len());
        let mut activated = 0usize;
        for entry in entries {
            let sender_ipk = entry.envelope.sender_ipk.0;
            let wid = entry.welcome_id.0;
            let ctx = self.ctx();
            match process_welcome_inbound_no_contacts(&ctx, sender_ipk, entry.envelope) {
                Ok(_) => {
                    processed.push(wid);
                    activated += 1;
                },
                // Duplicate (same Welcome at multiple K homes; KP already
                // consumed) or malformed — ack anyway so the home GCs.
                Err(_) => processed.push(wid),
            }
        }
        if !processed.is_empty() {
            let _ = self.dht.ack_welcomes(&processed).await;
        }
        Ok(activated)
    }

    /// Encrypt an application message for `to` under the founder group.
    /// Returns the wire envelope bytes (the relay-opaque `DispatchP` payload).
    fn encrypt(&mut self, to: [u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut group = self.groups.remove(&to).ok_or_else(|| anyhow!("no group for recipient"))?;
        let out = {
            let leaf = leaf_signer_for_group(&self.provider, &group, &self.ipk)?;
            let ctx = self.ctx();
            build_application_envelope_bytes(
                &ctx,
                &mut group,
                &leaf,
                &self.ipk,
                &to,
                plaintext,
                &self.ipk_signer,
            )
        };
        self.groups.insert(to, group);
        out.map_err(|e| anyhow!("build envelope: {e}"))
    }

    /// Decrypt an application envelope from `sender_ipk`. The group is
    /// looked up via the provider (joined earlier through `poll_welcomes`).
    fn decrypt(&self, sender_ipk: [u8; 32], envelope: &[u8]) -> Result<Vec<u8>> {
        let app = match MlsEnvelopeP::deser(envelope)? {
            MlsEnvelopeP::Application(a) => a,
            MlsEnvelopeP::Welcome(_) => bail!("expected Application envelope, got Welcome"),
        };
        let ctx = self.ctx();
        match process_application_inbound_for(&ctx, sender_ipk, &self.ipk, app)? {
            InboundDecoded::Application { plaintext, .. } => Ok(plaintext),
            InboundDecoded::ApplicationBuffered => {
                bail!("application buffered (epoch ahead); recipient not caught up")
            },
            InboundDecoded::ApplicationStale => bail!("application stale (epoch behind); dropped"),
            InboundDecoded::Welcome => bail!("expected Application decode, got Welcome"),
        }
    }
}

/// Establish + authenticate a `client/0` connection, returning the live
/// connection and the home's advertised DHT NodeId. Replicates
/// `quic::server::Relay::connect`'s handshake with an explicit signer (no
/// keystore): `Hello` -> `Challenge` -> `Proof` -> `Accept`.
async fn connect_home(
    addr: SocketAddr, server_name: &str, ca: &Path, ipk: [u8; 32], signer: &SigningKey,
) -> Result<(Connection, Option<[u8; 32]>)> {
    setup_crypto_provider()?;
    let roots = load_root_ca(&ca.to_path_buf())?;
    let client_cfg = build_client_cfg(ProtoRole::Client, &roots)?;
    // 0.0.0.0 (not 127.0.0.1) so the client can route to real relays on
    // public IPs, not just loopback sandbox relays.
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);

    let conn = endpoint.connect(addr, server_name)?.await?;

    let (mut tx, mut rx) = conn.open_bi().await?;
    CHandshakePacket::Hello { ipk: ipk.into() }.send(&mut tx).await?;

    let nonce = match SHSP::unpack(&mut rx).await? {
        SHSP::Challenge { nonce } => nonce,
        other => bail!("handshake: expected Challenge, got {other:?}"),
    };
    let msg = [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &*nonce].concat();
    let sig = signer.sign(&msg).to_bytes();
    CHandshakePacket::Proof { sig: sig.into() }.send(&mut tx).await?;

    let home_node_id = match SHSP::unpack(&mut rx).await? {
        SHSP::HandshakeResult(SHSRP::Accept { relay_node_id, .. }) => relay_node_id.map(|b| b.0),
        SHSP::HandshakeResult(SHSRP::Reject { reason }) => {
            bail!("relay rejected handshake: {reason}")
        },
        other => bail!("handshake: expected HandshakeResult, got {other:?}"),
    };

    Ok((conn, home_node_id))
}

fn env_var(k: &str) -> Result<String> {
    std::env::var(k).map_err(|_| anyhow!("missing env var {k}"))
}

fn next_ipk(args: &mut SplitWhitespace<'_>) -> Result<[u8; 32]> {
    let s = args.next().ok_or_else(|| anyhow!("missing ipk argument"))?;
    let v = hex::decode(s)?;
    v.try_into().map_err(|_| anyhow!("ipk must be 32 bytes"))
}
