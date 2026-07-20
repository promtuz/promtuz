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
