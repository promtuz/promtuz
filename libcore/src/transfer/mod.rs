//! P2P attachment transfer: chunked-manifest protocol for files too big for
//! the inline `Image` message (>256KB), carried over a direct link from
//! [`crate::p2p`] rather than the store-and-forward relay.

pub mod store;
pub mod wire;

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
/// v1 needs no per-pull auth handshake: the link is already consent-gated to a
/// paired contact, and the content-addressed `file_id` is the capability — you
/// can only pull a file whose manifest hash you were handed over the E2E
/// channel. An explicit IPK binding lands later.
pub async fn serve_link(link: crate::p2p::PeerLink) {
    loop {
        let (mut s, mut r) = match link.accept_stream().await {
            Ok(x) => x,
            Err(_) => break,
        };
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
}
