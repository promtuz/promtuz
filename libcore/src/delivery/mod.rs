use common::proto::client_rel::DispatchAckP;
use rusqlite::params;

use crate::db::outbox::OUTBOX_DB;
use crate::db::outbox::OpType;
use crate::db::outbox::OutboxRow;

/// Durability verdict for a dispatch attempt. `outcome_for_ack` is the single
/// ack→durability mapping shared by the live send path (Task 6) and the
/// reconciler (Task 7) so the "which ack retires the row" decision can't drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastOutcome {
    Durable,
    Queued,
    Reachable,
    Terminal,
    /// Reconciler-only: no ack came back within TTL. Never returned by
    /// `outcome_for_ack`.
    Silence,
}

/// Map a relay `DispatchAckP` to its durability verdict. Exhaustive on purpose:
/// a new ack variant must be a compile error here, not a silent miscategory.
pub fn outcome_for_ack(ack: &DispatchAckP) -> LastOutcome {
    use LastOutcome::*;
    match ack {
        DispatchAckP::Forwarded | DispatchAckP::Delivered => Durable,
        DispatchAckP::Queued => Queued,
        DispatchAckP::QueueFull | DispatchAckP::Error { .. } => Reachable,
        DispatchAckP::NotFound | DispatchAckP::InvalidSig => Terminal,
    }
}

// SQLite integers are i64; rusqlite's u64 binder rejects anything past
// i64::MAX. Real ms timestamps fit, but the u64::MAX "never/always due"
// sentinel would overflow — saturate so it stays i64::MAX, not a wrapped -1.
fn ms_i64(ms: u64) -> i64 {
    ms.min(i64::MAX as u64) as i64
}

pub fn enqueue(id: &[u8], op: OpType, target_ipk: Option<[u8; 32]>, payload: &[u8]) {
    OUTBOX_DB
        .lock()
        .execute(
            "INSERT INTO outbox (id, op_type, target_ipk, payload, created_at, next_attempt)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)
             ON CONFLICT(id) DO NOTHING",
            params![
                id,
                op as u8,
                target_ipk.as_ref().map(|a| a.as_slice()),
                payload,
                ms_i64(crate::utils::systime().as_millis() as u64),
            ],
        )
        .ok();
}

pub fn retire(id: &[u8]) {
    OUTBOX_DB.lock().execute("DELETE FROM outbox WHERE id = ?1", params![id]).ok();
}

pub fn due(now_ms: u64) -> Vec<OutboxRow> {
    let conn = OUTBOX_DB.lock();
    let mut stmt = conn
        .prepare("SELECT * FROM outbox WHERE state = 0 AND next_attempt <= ?1 ORDER BY created_at ASC")
        .expect("prepare due");
    stmt.query_map(params![ms_i64(now_ms)], OutboxRow::from_row)
        .expect("query due")
        .filter_map(|r| r.ok())
        .collect()
}

pub fn record_attempt(id: &[u8], next_attempt: u64) {
    OUTBOX_DB
        .lock()
        .execute(
            "UPDATE outbox SET attempts = attempts + 1, next_attempt = ?2 WHERE id = ?1",
            params![id, ms_i64(next_attempt)],
        )
        .ok();
}

pub fn mark_dead(id: &[u8]) {
    OUTBOX_DB.lock().execute("UPDATE outbox SET state = 1 WHERE id = ?1", params![id]).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_for_ack_maps_all_variants() {
        use LastOutcome::*;
        assert!(matches!(outcome_for_ack(&DispatchAckP::Delivered), Durable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Forwarded), Durable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Queued), Queued));
        assert!(matches!(outcome_for_ack(&DispatchAckP::QueueFull), Reachable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Error { reason: String::new() }), Reachable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::NotFound), Terminal));
        assert!(matches!(outcome_for_ack(&DispatchAckP::InvalidSig), Terminal));
    }

    #[test]
    fn outbox_enqueue_due_retire() {
        // db() calls process::exit(1) if PROMTUZ_DATA_DIR is unset; point it
        // at a scratch dir and wipe any prior run's db (incl. WAL siblings)
        // so `due` counts are deterministic.
        let dir = std::env::temp_dir().join("promtuz-outbox-test");
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["outbox.db", "outbox.db-wal", "outbox.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id = [1u8; 16];
        enqueue(&id, OpType::Message, Some([2u8; 32]), b"payload");
        assert_eq!(due(u64::MAX).len(), 1);

        // Re-enqueue of the same id is a silent no-op — still one row.
        enqueue(&id, OpType::Message, Some([2u8; 32]), b"payload");
        assert_eq!(due(u64::MAX).len(), 1);

        // Future backoff excludes the row from due-now.
        record_attempt(&id, u64::MAX);
        assert!(due(0).is_empty());

        // Dead rows never surface.
        mark_dead(&id);
        assert!(due(u64::MAX).is_empty());

        retire(&id);
        assert!(due(u64::MAX).is_empty());
    }
}
