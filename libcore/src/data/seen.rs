//! Delivery dedup ledger. A relay stores a dispatch at K home relays; when we
//! reconnect to a *different* home than the one that delivered live, it
//! redelivers the same dispatch. Re-decrypting is fatal — MLS forward secrecy
//! already consumed that message's ratchet key, so openmls throws
//! SecretReuseError. Keyed on `(sender, dispatch_id)` — a dispatch id is
//! minted by its sender, so that pair names a dispatch uniquely without
//! resolving a conversation. That matters: the check runs *pre-decrypt*,
//! where a Welcome or an envelope for a group we do not hold yet has no
//! conversation to resolve against.

use crate::db::messages::MESSAGES_DB;

pub struct Seen;

impl Seen {
    /// Have we already decrypted this dispatch?
    pub fn contains(sender: &[u8; 32], dispatch_id: &[u8]) -> bool {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT 1 FROM seen_dispatch WHERE sender_ipk = ?1 AND dispatch_id = ?2",
            (sender.as_slice(), dispatch_id),
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Record a dispatch as decrypted. Idempotent.
    // ponytail: grows with lifetime message count (~48B/row); a prune past the
    // relay queue TTL can come later — dispatch_ids are never reused, so a
    // stale row is only space, never a correctness risk.
    pub fn record(sender: &[u8; 32], dispatch_id: &[u8], now_secs: u64) {
        let conn = MESSAGES_DB.lock();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO seen_dispatch (sender_ipk, dispatch_id, seen_at) \
             VALUES (?1, ?2, ?3)",
            (sender.as_slice(), dispatch_id, now_secs),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::db::messages::open_in_memory;

    #[test]
    fn record_then_contains_roundtrips_and_ignores_dup() {
        let conn = open_in_memory();
        let sender = [4u8; 32];
        let did = [9u8; 16];
        let seen = |c: &rusqlite::Connection| {
            c.query_row(
                "SELECT 1 FROM seen_dispatch WHERE sender_ipk = ?1 AND dispatch_id = ?2",
                (sender.as_slice(), did.as_slice()),
                |_| Ok(()),
            )
            .is_ok()
        };
        assert!(!seen(&conn), "unseen before record");
        let ins = |c: &rusqlite::Connection| {
            c.execute(
                "INSERT OR IGNORE INTO seen_dispatch (sender_ipk, dispatch_id, seen_at) VALUES (?1, ?2, 0)",
                (sender.as_slice(), did.as_slice()),
            )
            .unwrap()
        };
        assert_eq!(ins(&conn), 1, "first insert lands");
        assert!(seen(&conn), "seen after record");
        assert_eq!(ins(&conn), 0, "duplicate insert is a no-op");
    }
}
