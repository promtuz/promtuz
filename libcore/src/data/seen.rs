//! Delivery dedup, checked before decryption since MLS cannot decrypt a dispatch twice. Keyed by
//! (sender, dispatch_id), because before decryption there may be no conversation to resolve.

use crate::state::core;

pub struct Seen;

impl Seen {
    pub fn contains(sender: &[u8; 32], dispatch_id: &[u8]) -> bool {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT 1 FROM seen_dispatch WHERE sender_ipk = ?1 AND dispatch_id = ?2",
            (sender.as_slice(), dispatch_id),
            |_| Ok(()),
        )
        .is_ok()
    }

    pub fn record(sender: &[u8; 32], dispatch_id: &[u8], now_secs: u64) {
        let conn = core().db.messages().lock();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO seen_dispatch (sender_ipk, dispatch_id, seen_at) \
             VALUES (?1, ?2, ?3)",
            (sender.as_slice(), dispatch_id, now_secs),
        );
    }
}
