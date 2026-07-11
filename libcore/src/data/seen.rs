//! Delivery dedup ledger. A relay stores a dispatch at K home relays; when we
//! reconnect to a *different* home than the one that delivered live, it
//! redelivers the same dispatch. Re-decrypting is fatal — MLS forward secrecy
//! already consumed that message's ratchet key, so openmls throws
//! SecretReuseError. Keyed on the outer `(peer, dispatch_id)`, so the check is
//! pre-decrypt and covers every payload type (text, control, welcome).

use crate::db::messages::MESSAGES_DB;

pub struct Seen;

impl Seen {
    /// Have we already decrypted this dispatch?
    pub fn contains(peer_ipk: &[u8; 32], dispatch_id: &[u8]) -> bool {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT 1 FROM seen_dispatch WHERE peer_ipk = ?1 AND dispatch_id = ?2",
            (peer_ipk.as_slice(), dispatch_id),
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Record a dispatch as decrypted. Idempotent.
    // ponytail: grows with lifetime message count (~48B/row); a prune past the
    // relay queue TTL can come later — dispatch_ids are never reused, so a
    // stale row is only space, never a correctness risk.
    pub fn record(peer_ipk: &[u8; 32], dispatch_id: &[u8], now_secs: u64) {
        let conn = MESSAGES_DB.lock();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO seen_dispatch (peer_ipk, dispatch_id, seen_at) \
             VALUES (?1, ?2, ?3)",
            (peer_ipk.as_slice(), dispatch_id, now_secs),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::db::messages::open_in_memory;

    #[test]
    fn record_then_contains_roundtrips_and_ignores_dup() {
        let conn = open_in_memory();
        let peer = [4u8; 32];
        let did = [9u8; 16];
        let seen = |c: &rusqlite::Connection| {
            c.query_row(
                "SELECT 1 FROM seen_dispatch WHERE peer_ipk = ?1 AND dispatch_id = ?2",
                (peer.as_slice(), did.as_slice()),
                |_| Ok(()),
            )
            .is_ok()
        };
        assert!(!seen(&conn), "unseen before record");
        let ins = |c: &rusqlite::Connection| {
            c.execute(
                "INSERT OR IGNORE INTO seen_dispatch (peer_ipk, dispatch_id, seen_at) VALUES (?1, ?2, 0)",
                (peer.as_slice(), did.as_slice()),
            )
            .unwrap()
        };
        assert_eq!(ins(&conn), 1, "first insert lands");
        assert!(seen(&conn), "seen after record");
        assert_eq!(ins(&conn), 0, "duplicate insert is a no-op");
    }
}
