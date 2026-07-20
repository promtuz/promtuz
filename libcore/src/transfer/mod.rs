//! P2P attachment transfer: chunked-manifest protocol for files too big for
//! the inline `Image` message (>256KB), carried over a direct link from
//! [`crate::p2p`] rather than the store-and-forward relay.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

pub mod auth;
pub mod store;
pub mod wire;

/// Auto-download ceiling: bigger offers wait for a user tap even on wifi.
pub const AUTO_MAX: u64 = 5 * 1024 * 1024;

/// Abandoned receiver partials older than this are reaped by [`gc`].
const DEAD_PARTIAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Periodic housekeeping. Drops sender retention rows whose TTL has passed —
/// a DB-row delete ONLY: the retained `path` is the user's own source file
/// (the photo/document they chose to send) and is never unlinked. Then reaps
/// abandoned receiver partials, unlinking only their junk `.part` bytes; a
/// delivered `DONE` partial (the file the user keeps) is spared.
pub fn gc(now: u64) {
    let _ = store::retention_gc(now);
    let _ = store::gc_dead_partials(now.saturating_sub(DEAD_PARTIAL_TTL_SECS));
}

/// Whether to pull an offered attachment without a user tap: only from a paired
/// contact, over a trusted (un-metered) network, and at or below [`AUTO_MAX`].
/// Pure policy so the receive arm and its test share one rule.
pub fn should_auto_download(ipk: &[u8; 32], size: u64, on_wifi: bool) -> bool {
    on_wifi && size <= AUTO_MAX && crate::data::contact::Contact::is_paired(ipk)
}

/// Builds the manifest for `path`, retains it (and the source location) so we
/// keep serving pulls until `ttl_secs` elapses, and returns the offer's
/// `(file_id, size)`.
pub fn prepare_send(path: &str, ttl_secs: u64) -> anyhow::Result<([u8; 32], u64)> {
    let m = wire::Manifest::from_file(path)?;
    let file_id = m.file_id();
    let size = m.total_size;
    let expires = crate::utils::systime().as_secs() + ttl_secs;
    store::retention_put(&file_id, path, size, m.chunk_size, &postcard::to_allocvec(&m)?, expires)?;
    Ok((file_id, size))
}

/// Answer pulls over `link` until the peer stops opening streams: read one
/// [`wire::Pull`] per bi-stream, then either reply [`wire::ServeResp::Gone`]
/// (we no longer retain it) or frame the [`wire::Manifest`] and stream the
/// requested chunk bytes raw from `have` to EOF.
///
/// The framed manifest is the only length-delimited part; the chunk bytes ride
/// after it unframed, since the puller sizes and counts them from that manifest.
///
/// Every stream starts with the mutual [`auth`] handshake pinning the peer's
/// IPK to this connection's TLS key; a stream that fails it is dropped before
/// any pull is read.
pub async fn serve_link(link: crate::p2p::PeerLink) {
    let local = match auth::local_auth() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("transfer: cannot serve without local auth: {e}");
            return;
        },
    };
    serve_streams(link, local).await
}

/// [`serve_link`] minus the process-global identity, so a test can drive two
/// in-process endpoints with distinct constructed identities.
async fn serve_streams(link: crate::p2p::PeerLink, local: wire::Auth) {
    loop {
        let (mut s, mut r) = match link.accept_stream().await {
            Ok(x) => x,
            Err(_) => break,
        };
        if let Err(e) = auth::exchange(&link.conn, &mut s, &mut r, link.ipk, &local).await {
            log::warn!("transfer: stream auth failed: {e}");
            continue;
        }
        let pull: wire::Pull = match wire::read_frame(&mut r).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        match store::retention_get(&pull.file_id) {
            None => {
                let _ = wire::write_frame(&mut s, &wire::ServeResp::Gone).await;
                let _ = s.finish();
            },
            Some(ret) => {
                // A stored manifest that won't decode is our corruption, not the
                // peer's; treat it as gone rather than panic this detached loop.
                let manifest: wire::Manifest = match postcard::from_bytes(&ret.manifest) {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("transfer: undecodable retained manifest: {e}");
                        let _ = wire::write_frame(&mut s, &wire::ServeResp::Gone).await;
                        let _ = s.finish();
                        continue;
                    },
                };
                let _ = wire::write_frame(&mut s, &wire::ServeResp::Manifest(manifest)).await;
                if let Ok(mut f) = std::fs::File::open(&ret.path) {
                    use std::io::{Read, Seek, SeekFrom};
                    let _ = f.seek(SeekFrom::Start(pull.have as u64 * ret.chunk_size as u64));
                    let mut buf = vec![0u8; ret.chunk_size as usize];
                    loop {
                        let n = match f.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
                let _ = s.finish();
            },
        }
    }
}

/// Pulls in flight by `file_id`. Two concurrent `download`s (auto-download
/// racing a manual tap) must not co-write one partial file; the loser no-ops
/// and the UI follows the winner through the `partials` doorbell.
static DOWNLOADING: Lazy<Mutex<HashSet<[u8; 32]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Releases the [`DOWNLOADING`] slot on every exit path — a leaked entry
/// would wedge the file_id forever.
struct PullGuard([u8; 32]);
impl Drop for PullGuard {
    fn drop(&mut self) {
        DOWNLOADING.lock().remove(&self.0);
    }
}

/// Pull `file_id` from the contact who offered it: resolve the sender from
/// the media row, dial (or reuse) the P2P link, and run the resumable pull.
/// No-op when the file is already downloaded or a pull is in flight.
pub async fn download(file_id: [u8; 32]) -> anyhow::Result<()> {
    if store::partial_get(&file_id).is_some_and(|p| p.state == store::DONE) {
        return Ok(());
    }
    if !DOWNLOADING.lock().insert(file_id) {
        return Ok(());
    }
    let _guard = PullGuard(file_id);
    let peer = crate::data::media::sender_of(&file_id)?
        .ok_or_else(|| anyhow::anyhow!("no media row for that file_id"))?;
    let link = match crate::p2p::link(peer).await {
        Ok(l) => l,
        Err(e) => {
            // Sender unreachable — offline, or the punch/TURN path is exhausted
            // for a large file. Reverse-wake them and hold; the receiver retries
            // on reconnect or a user tap. Not an error: the UI reads HELD.
            log::info!(
                "transfer: {} unreachable ({e}); reverse-waking, holding {}",
                hex::encode(&peer[..4]),
                hex::encode(&file_id[..4]),
            );
            let _ = crate::messaging::send_control_wake(
                peer,
                common::proto::mls_wire::AppPayload::FileWant { file_id },
            )
            .await;
            hold(&file_id, peer);
            return Ok(());
        },
    };
    pull(&link, file_id, &auth::local_auth()?).await
}

/// Mark a pull as HELD (sender offline, reverse-wake sent): upsert the partial
/// so `get_media` surfaces the held state to the UI. Mirrors [`pull`]'s upsert
/// but with no manifest yet — the real bytes arrive once the sender comes back.
fn hold(file_id: &[u8; 32], peer: [u8; 32]) {
    let mut p = store::partial_get(file_id).unwrap_or(store::Partial {
        file_id: *file_id,
        source_ipk: peer,
        total: 0,
        chunk_size: 0,
        manifest: None,
        have: 0,
        state: store::HELD,
        path: store::partial_path(file_id),
        updated_at: 0,
    });
    p.state = store::HELD;
    p.source_ipk = peer;
    p.updated_at = crate::utils::systime().as_secs();
    let _ = store::partial_put(&p);
}

/// Handle an inbound reverse-wake: a contact wants a file we offered but
/// couldn't reach us for. The platform push already revived the app; make sure
/// our P2P listener is live so their retry-dial can land (they drive the connect).
pub fn on_file_want(peer: [u8; 32], file_id: [u8; 32]) {
    log::info!(
        "transfer: FileWant from {} for {}",
        hex::encode(&peer[..4]),
        hex::encode(&file_id[..4]),
    );
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::p2p::ensure_endpoint() {
            log::warn!("transfer: P2P endpoint bring-up failed: {e}");
        }
    });
}

/// The wire+disk half of [`download`] over an already-open link, split out so
/// a test can drive it against [`serve_link`] on a direct loopback pair
/// (acquiring a real link needs the full punch choreography).
///
/// Crash-safety contract: a chunk's bytes are synced to disk BEFORE the
/// `have` watermark covering them is persisted, so a resume never trusts a
/// watermark ahead of real bytes — the worst crash re-pulls one chunk.
async fn pull(
    link: &crate::p2p::PeerLink, file_id: [u8; 32], local: &wire::Auth,
) -> anyhow::Result<()> {
    let have0 = store::partial_get(&file_id).map(|p| p.have).unwrap_or(0);
    let (mut s, mut r) = link.open_stream().await?;
    auth::exchange(&link.conn, &mut s, &mut r, link.ipk, local).await?;
    wire::write_frame(&mut s, &wire::Pull { file_id, have: have0 }).await?;
    s.finish()?;
    let manifest = match wire::read_frame::<wire::ServeResp>(&mut r).await? {
        wire::ServeResp::Manifest(m) => m,
        wire::ServeResp::Gone => {
            if let Some(mut p) = store::partial_get(&file_id) {
                p.state = store::FAILED;
                p.updated_at = crate::utils::systime().as_secs();
                let _ = store::partial_put(&p);
            }
            anyhow::bail!("sender no longer retains the file");
        },
    };
    // Fail closed before any bytes land: the manifest must be the exact one
    // the content-addressed file_id commits to, and self-consistent so a bad
    // peer can't drive the chunk math to over-allocate or mis-index.
    anyhow::ensure!(manifest.file_id() == file_id, "manifest does not match file_id");
    anyhow::ensure!(
        manifest.chunk_size > 0 && manifest.chunk_size as usize <= wire::CHUNK_SIZE,
        "bad chunk_size"
    );
    anyhow::ensure!(
        manifest.chunks.len() as u64 == manifest.total_size.div_ceil(manifest.chunk_size as u64),
        "chunk count does not match total_size"
    );
    anyhow::ensure!(have0 as usize <= manifest.chunks.len(), "partial ahead of manifest");

    let path = store::partial_path(&file_id);
    let mut part = store::Partial {
        file_id,
        source_ipk: link.ipk,
        total: manifest.total_size,
        chunk_size: manifest.chunk_size,
        manifest: Some(postcard::to_allocvec(&manifest)?),
        have: have0,
        state: store::ACTIVE,
        path: path.clone(),
        updated_at: crate::utils::systime().as_secs(),
    };
    store::partial_put(&part)?;

    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().create(true).write(true).read(true).open(&path)?;
    f.seek(SeekFrom::Start(have0 as u64 * manifest.chunk_size as u64))?;
    let mut buf = vec![0u8; manifest.chunk_size as usize];
    for idx in have0 as usize..manifest.chunks.len() {
        let expect = if idx + 1 == manifest.chunks.len() {
            (manifest.total_size - idx as u64 * manifest.chunk_size as u64) as usize
        } else {
            manifest.chunk_size as usize
        };
        r.read_exact(&mut buf[..expect]).await?;
        anyhow::ensure!(
            *blake3::hash(&buf[..expect]).as_bytes() == manifest.chunks[idx],
            "chunk {idx} hash mismatch"
        );
        f.write_all(&buf[..expect])?;
        f.flush()?;
        f.sync_data()?;
        part.have = idx as u32 + 1;
        part.updated_at = crate::utils::systime().as_secs();
        store::partial_put(&part)?; // doorbell → UI progress
    }
    // No rename on completion: state==DONE over the .part path IS the promote —
    // a single DB flip with no rename/DB-ordering crash window; get_media only
    // exposes local_path once DONE.
    part.state = store::DONE;
    part.updated_at = crate::utils::systime().as_secs();
    store::partial_put(&part)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_send_retains_manifest() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let path = std::env::temp_dir().join("promtuz-prepare_send.bin");
        std::fs::write(&path, vec![0x11u8; 300 * 1024]).unwrap();

        let (file_id, size) = prepare_send(path.to_str().unwrap(), 3600).unwrap();
        assert_eq!(size, 300 * 1024);
        assert!(store::retention_get(&file_id).is_some());
    }

    #[test]
    fn auto_download_only_paired_wifi_and_small() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        use crate::data::contact::Contact;
        let paired = [0xa1u8; 32];
        Contact::save_pending(paired, "peer".into()).unwrap();
        Contact::mark_paired(&paired);
        assert!(Contact::is_paired(&paired));
        let unpaired = [0xa2u8; 32];

        assert!(should_auto_download(&paired, AUTO_MAX, true), "paired + wifi + at-cap");
        assert!(!should_auto_download(&unpaired, AUTO_MAX, true), "unpaired never");
        assert!(!should_auto_download(&paired, AUTO_MAX, false), "metered never");
        assert!(!should_auto_download(&paired, AUTO_MAX + 1, true), "oversize never");
    }
}

#[cfg(test)]
mod download_resume {
    use std::net::Ipv6Addr;

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::p2p::PeerLink;

    /// Both loopback endpoints present this TLS key ([`linked_pair`] builds
    /// them from it), so a test Auth's `tls_pub` must vouch for it.
    const TLS_SEED: [u8; 32] = [7u8; 32];

    /// A per-endpoint identity for the handshake: the IPK from `ipk_seed`
    /// signing the binding over the shared loopback TLS key, saved as a
    /// paired contact so `verify_auth`'s consent check passes.
    fn paired_identity(ipk_seed: [u8; 32]) -> wire::Auth {
        use ed25519_dalek::Signer;
        let ipk_key = SigningKey::from_bytes(&ipk_seed);
        let tls_pub = SigningKey::from_bytes(&TLS_SEED).verifying_key().to_bytes();
        let msg = crate::quic::peer_config::ipk_binding_message(&tls_pub);
        let a = wire::Auth {
            ipk: ipk_key.verifying_key().to_bytes(),
            tls_pub,
            sig: ipk_key.sign(&msg).to_bytes(),
        };
        crate::data::contact::Contact::save_pending(a.ipk, "peer".into()).unwrap();
        crate::data::contact::Contact::mark_paired(&a.ipk);
        a
    }

    /// Two directly-connected peer endpoints on loopback — the real QUIC
    /// stack minus the punch layer, which a unit test can't drive. Each
    /// link's `ipk` is the peer that side expects on its streams.
    async fn linked_pair(
        a_expects: [u8; 32], b_expects: [u8; 32],
    ) -> (PeerLink, PeerLink, quinn::Endpoint, quinn::Endpoint) {
        let _ = common::quic::config::setup_crypto_provider();
        let key = SigningKey::from_bytes(&TLS_SEED);
        let (server_cfg, client_cfg) = crate::quic::peer_config::test_peer_configs(&key).unwrap();
        let ep_a = quinn::Endpoint::server(server_cfg, (Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        let mut ep_b = quinn::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        ep_b.set_default_client_config(client_cfg);
        let dial = ep_b.connect(ep_a.local_addr().unwrap(), "peer").unwrap();
        let (conn_a, conn_b) = tokio::join!(
            async { ep_a.accept().await.unwrap().accept().unwrap().await.unwrap() },
            async { dial.await.unwrap() },
        );
        (
            crate::p2p::test_link(conn_a, a_expects),
            crate::p2p::test_link(conn_b, b_expects),
            ep_a,
            ep_b,
        )
    }

    #[tokio::test]
    async fn pull_verifies_resumes_and_promotes() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id_a = paired_identity([51u8; 32]);
        let id_b = paired_identity([52u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        // Distinct per-chunk content so a mis-aligned resume can't verify.
        let src = std::env::temp_dir().join("promtuz-dl-src.bin");
        let mut bytes = vec![0xaau8; 300 * 1024];
        bytes[wire::CHUNK_SIZE..].fill(0xbb);
        std::fs::write(&src, &bytes).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();

        // Fresh pull: every chunk lands, verifies, and the partial promotes.
        pull(&link_b, file_id, &id_b).await.unwrap();
        let p = store::partial_get(&file_id).unwrap();
        assert_eq!(p.state, store::DONE);
        assert_eq!(p.have, 2);
        assert_eq!(std::fs::read(&p.path).unwrap(), bytes);
        assert_eq!(wire::Manifest::from_file(&p.path).unwrap().file_id(), file_id);

        // Resumed pull moves ONLY the tail: pre-seed have=1 with garbage in
        // chunk 0's region — re-transferring chunk 0 would either overwrite
        // the garbage or fail the hash check on mis-aligned bytes.
        let src2 = std::env::temp_dir().join("promtuz-dl-src2.bin");
        let mut bytes2 = vec![0x11u8; 300 * 1024];
        bytes2[wire::CHUNK_SIZE..].fill(0x22);
        std::fs::write(&src2, &bytes2).unwrap();
        let (file_id2, _) = prepare_send(src2.to_str().unwrap(), 3600).unwrap();
        let path2 = store::partial_path(&file_id2);
        std::fs::write(&path2, vec![0x99u8; wire::CHUNK_SIZE]).unwrap();
        store::partial_put(&store::Partial {
            file_id: file_id2,
            source_ipk: [1; 32],
            total: 300 * 1024,
            chunk_size: wire::CHUNK_SIZE as u32,
            manifest: None,
            have: 1,
            state: store::ACTIVE,
            path: path2.clone(),
            updated_at: 0,
        })
        .unwrap();

        pull(&link_b, file_id2, &id_b).await.unwrap();
        assert_eq!(store::partial_get(&file_id2).unwrap().state, store::DONE);
        let got = std::fs::read(&path2).unwrap();
        assert_eq!(got.len(), 300 * 1024);
        assert!(got[..wire::CHUNK_SIZE].iter().all(|&b| b == 0x99), "chunk 0 was re-transferred");
        assert_eq!(&got[wire::CHUNK_SIZE..], &bytes2[wire::CHUNK_SIZE..]);
    }

    #[tokio::test]
    async fn serve_refuses_wrong_ipk_before_any_chunk() {
        let dir = std::env::temp_dir().join("promtuz-download-resume-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id_a = paired_identity([53u8; 32]);
        let id_b = paired_identity([54u8; 32]);
        // The imposter is even a paired contact with a valid binding — only
        // its IPK differs from the peer the server expects on this link.
        let imposter = paired_identity([55u8; 32]);
        let (link_a, link_b, _ep_a, _ep_b) = linked_pair(id_b.ipk, id_a.ipk).await;
        tokio::spawn(serve_streams(link_a, id_a));

        let src = std::env::temp_dir().join("promtuz-dl-src3.bin");
        std::fs::write(&src, vec![0x33u8; 300 * 1024]).unwrap();
        let (file_id, _) = prepare_send(src.to_str().unwrap(), 3600).unwrap();

        assert!(pull(&link_b, file_id, &imposter).await.is_err());
        assert!(store::partial_get(&file_id).is_none(), "no state before auth passes");
        assert!(
            !std::path::Path::new(&store::partial_path(&file_id)).exists(),
            "no bytes before auth passes"
        );
    }
}
