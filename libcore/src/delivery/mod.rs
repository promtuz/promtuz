use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::SRelayPacket;
use log::debug;
use log::warn;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::pack::Unpacker;
use rusqlite::params;

use crate::data::message::Message;
use crate::data::message::STATUS_FAILED;
use crate::data::message::STATUS_SENT;
use crate::db::outbox::OUTBOX_DB;
use crate::db::outbox::OpType;
use crate::db::outbox::OutboxRow;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::KpOutcomeFilter;

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
        // `Queued` is the relay's local-fallback ack, returned only AFTER a
        // `put_sync` fsync (see forward.rs::store_in_rocks) — a durable handoff,
        // so the message is "sent" even if the recipient is offline. Treating it
        // as non-durable was the "stuck pending until the recipient logs in" bug.
        DispatchAckP::Forwarded { .. }
        | DispatchAckP::Delivered { .. }
        | DispatchAckP::Queued { .. } => Durable,
        DispatchAckP::QueueFull | DispatchAckP::Error { .. } => Reachable,
        DispatchAckP::NotFound | DispatchAckP::InvalidSig => Terminal,
    }
}

/// Relay acceptance time accompanies every durable dispatch acknowledgement.
pub fn accepted_at_secs(ack: &DispatchAckP) -> Option<u64> {
    match ack {
        DispatchAckP::Forwarded { accepted_at_ms }
        | DispatchAckP::Delivered { accepted_at_ms }
        | DispatchAckP::Queued { accepted_at_ms } => Some(accepted_at_ms / 1_000),
        _ => None,
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

/// Drop every queued op targeting this peer (forget-contact cascade).
pub fn forget_target(ipk: &[u8; 32]) {
    OUTBOX_DB
        .lock()
        .execute("DELETE FROM outbox WHERE target_ipk = ?1", params![ipk.as_slice()])
        .ok();
}

/// Count of pending (state = 0) ops queued for this peer (diagnostics read).
pub fn pending_ops_for(ipk: &[u8; 32]) -> u32 {
    OUTBOX_DB
        .lock()
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE target_ipk = ?1 AND state = 0",
            params![ipk.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u32)
        .unwrap_or(0)
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

// ponytail: calibration knobs — retry cadence and death thresholds, tuned by
// feel not measurement. Adjust when real relay behaviour is observed.
const BASE_BACKOFF_MS: u64 = 1_000; // first retry after ~1s
const CAP_BACKOFF_MS: u64 = 300_000; // backoff capped at 5 min
const QUEUED_ESCALATION_MAX: u32 = 5; // Queued IS delivery in single-relay/dev; retire after N reconnects
const DEAD_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000; // non-message silence past 7d dies
const MESSAGE_SILENCE_MAX: u32 = 6; // fail a message after this many no-ack retries (~2min)

/// What a pending row does after this attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    KeepRetrying,
    Retire,
    Dead,
}

/// The terminal-decision policy — the whole reliability contract. Never fails a
/// message prematurely; never lets a persistently-`Reachable` op die (the
/// original KP-publish bug).
pub fn classify(op: OpType, last: LastOutcome, attempts: u32, age_ms: u64) -> Next {
    match last {
        LastOutcome::Durable | LastOutcome::Terminal => Next::Retire,
        LastOutcome::Queued =>
            if attempts >= QUEUED_ESCALATION_MAX { Next::Retire } else { Next::KeepRetrying },
        LastOutcome::Reachable => Next::KeepRetrying, // a NEGATIVE response still proves reachability — never kill
        // attempts-gated, not wall-clock, so an offline-queued msg isn't failed on reconnect
        LastOutcome::Silence => match op {
            OpType::Message if attempts >= MESSAGE_SILENCE_MAX => Next::Dead,
            _ if age_ms > DEAD_TTL_MS => Next::Dead,
            _ => Next::KeepRetrying,
        },
    }
}

// Exponential backoff. A plain `BASE << attempts` overflows u64 for large
// `attempts`; cap the shift AND the value.
fn next_backoff(attempts: u32) -> u64 {
    let shift = attempts.min(32); // 1000<<32 fits u64; caps well above CAP anyway
    (BASE_BACKOFF_MS << shift).min(CAP_BACKOFF_MS)
}

/// Re-dispatch every durably-queued `due` row over the live relay connection.
/// Called once per reconnect from `quic::server::handle`. No connection →
/// return (next reconnect retries); never marks a row Silence for a stream we
/// simply never opened.
pub async fn reconcile() {
    let now = crate::utils::systime().as_millis() as u64;
    let (conn, dht_client) = {
        let g = crate::state::RELAY.read();
        (
            g.as_ref().and_then(|r| r.connection.clone()),
            g.as_ref().and_then(|r| r.dht_client.clone()),
        )
    };
    let Some(conn) = conn else { return };

    for row in due(now) {
        let op = OpType::from_u8(row.op_type).unwrap_or(OpType::Message);
        let mut accepted_timestamp = None;
        let outcome = match op {
            OpType::KpPublish => {
                let Some(dht) = dht_client.clone() else { continue }; // no dht client → retry next reconnect
                let Ok(recs) = Vec::<KeyPackageRecord>::deser(&row.payload) else {
                    retire(&row.id); // poison payload can never publish — drop it
                    continue;
                };
                match dht.publish_keypackages(&recs, KpOutcomeFilter::Default).await {
                    Ok(()) => LastOutcome::Durable,
                    // Relay answered but the DHT isn't ready — Reachable keeps
                    // retrying forever, never dies. This is THE KP-bug fix.
                    Err(_) => LastOutcome::Reachable,
                }
            },
            // Message/Welcome ride the framed-Dispatch stream. Re-send the STORED
            // framed bytes verbatim (already `.pack()`-framed from Task 6). Any
            // open/write/finish/read error, or a non-DispatchAck reply, reads as
            // Silence (transport drop / no answer).
            _ => match conn.open_bi().await {
                Ok((mut send, mut recv)) => {
                    if send.write_all(&row.payload).await.is_ok()
                        && send.finish().is_ok()
                        && let Ok(SRelayPacket::DispatchAck(ack)) =
                            SRelayPacket::unpack(&mut recv).await
                    {
                        accepted_timestamp = accepted_at_secs(&ack);
                        outcome_for_ack(&ack)
                    } else {
                        LastOutcome::Silence
                    }
                },
                Err(_) => LastOutcome::Silence,
            },
        };

        let age = now.saturating_sub(row.created_at);
        match classify(op, outcome, row.attempts, age) {
            Next::Retire => {
                retire(&row.id);
                // Mirror the live send path onto the message the UI reads: a
                // row retired on the async path must leave its message `sent`
                // (Durable/Queued) or `failed` (Terminal), else an
                // offline-then-delivered message stays pending forever and a
                // rejected one fails silently. KpPublish has no message row.
                if matches!(op, OpType::Message) {
                    if matches!(outcome, LastOutcome::Terminal) {
                        mark_message_failed(&row.id, "relay rejected the message");
                    } else {
                        mark_message_sent(
                            &row.id,
                            accepted_timestamp.expect("durable dispatch ack has timestamp"),
                        );
                    }
                }
            },
            Next::Dead => {
                mark_dead(&row.id);
                if matches!(op, OpType::Message) {
                    warn!("MESSAGE: {} failed after {} attempts", hex::encode(&row.id[..row.id.len().min(4)]), row.attempts);
                    mark_message_failed(&row.id, "undeliverable after repeated retries");
                }
            },
            Next::KeepRetrying => {
                if matches!(op, OpType::Message) {
                    debug!("MESSAGE: {} still pending — {outcome:?} (attempt {})", hex::encode(&row.id[..row.id.len().min(4)]), row.attempts);
                }
                record_attempt(&row.id, now + next_backoff(row.attempts));
            },
        }
    }
}

/// Flip the message keyed by `dispatch_id` (the outbox row id) to `sent` and
/// emit the UI event, mirroring the live send path's Durable arm. No-op if no
/// such message (e.g. a non-Message op).
fn mark_message_sent(dispatch_id: &[u8], timestamp: u64) {
    if let Some(m) = Message::mark_by_dispatch_id(dispatch_id, STATUS_SENT, Some(timestamp)) {
        MessageEv::Sent { id: m.id, to: m.peer_ipk, content: m.content, timestamp: m.timestamp }
            .emit();
    }
}

/// Flip the message keyed by `dispatch_id` to `failed` and emit `Failed`,
/// mirroring the live send path's Terminal arm so a rejected/undeliverable
/// message doesn't fail silently.
fn mark_message_failed(dispatch_id: &[u8], reason: &str) {
    if let Some(m) = Message::mark_by_dispatch_id(dispatch_id, STATUS_FAILED, None) {
        MessageEv::Failed { id: m.id, to: m.peer_ipk, reason: reason.into() }.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_for_ack_maps_all_variants() {
        use LastOutcome::*;
        assert!(matches!(outcome_for_ack(&DispatchAckP::Delivered { accepted_at_ms: 1 }), Durable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Forwarded { accepted_at_ms: 1 }), Durable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Queued { accepted_at_ms: 1 }), Durable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::QueueFull), Reachable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::Error { reason: String::new() }), Reachable));
        assert!(matches!(outcome_for_ack(&DispatchAckP::NotFound), Terminal));
        assert!(matches!(outcome_for_ack(&DispatchAckP::InvalidSig), Terminal));
    }

    #[test]
    fn reachable_never_dies_even_when_ancient() {
        // THE KP-bug guard: a DhtUnavailable kp_publish, however old or
        // however many attempts, keeps retrying — never Dead, never Retire.
        assert!(matches!(
            classify(OpType::KpPublish, LastOutcome::Reachable, 9999, u64::MAX),
            Next::KeepRetrying
        ));
    }

    #[test]
    fn only_silence_past_ttl_dies() {
        assert!(matches!(
            classify(OpType::Message, LastOutcome::Silence, 0, DEAD_TTL_MS + 1),
            Next::Dead
        ));
        assert!(matches!(
            classify(OpType::Message, LastOutcome::Silence, 0, 0),
            Next::KeepRetrying
        ));
    }

    #[test]
    fn message_silence_dies_after_bounded_attempts() {
        assert!(matches!(classify(OpType::Message, LastOutcome::Silence, MESSAGE_SILENCE_MAX, 0), Next::Dead));
        assert!(matches!(classify(OpType::Message, LastOutcome::Silence, MESSAGE_SILENCE_MAX - 1, 0), Next::KeepRetrying));
    }

    #[test]
    fn queued_retires_after_bounded_escalation() {
        assert!(matches!(
            classify(OpType::Message, LastOutcome::Queued, QUEUED_ESCALATION_MAX, 0),
            Next::Retire
        ));
        assert!(matches!(
            classify(OpType::Message, LastOutcome::Queued, 0, 0),
            Next::KeepRetrying
        ));
    }

    #[test]
    fn durable_retires() {
        assert!(matches!(classify(OpType::Message, LastOutcome::Durable, 0, 0), Next::Retire));
    }

    #[test]
    fn next_backoff_is_monotonic_and_capped() {
        assert_eq!(next_backoff(0), BASE_BACKOFF_MS);
        // Large attempts saturate to the cap with no panic/overflow.
        assert_eq!(next_backoff(100), CAP_BACKOFF_MS);
        for a in 0..64 {
            assert!(next_backoff(a) <= CAP_BACKOFF_MS);
        }
    }

    #[test]
    fn kp_publish_stays_pending_when_dht_unavailable() {
        let dir = std::env::temp_dir().join("promtuz-outbox-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let id = b"kp-stays-pending"; // unique id → robust to the other outbox test's rows
        retire(id); // clean slate for this id
        enqueue(id, OpType::KpPublish, None, b"records");
        record_attempt(id, 0); // a failed publish attempt — still due-now
        assert_eq!(
            due(u64::MAX).iter().filter(|r| r.id == id).count(),
            1,
            "KpPublish must stay pending after a failed attempt"
        );
        retire(id); // cleanup
    }

    #[test]
    fn outbox_enqueue_due_retire() {
        // db() calls process::exit(1) if PROMTUZ_DATA_DIR is unset; point it at a
        // scratch dir. OUTBOX_DB is a process-global shared connection, so other
        // tests write to it concurrently — filter every assertion by this id.
        let dir = std::env::temp_dir().join("promtuz-outbox-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let id = [1u8; 16];
        let mine = |now: u64| due(now).into_iter().filter(|r| r.id == id).count();
        retire(&id); // clean slate for this id
        enqueue(&id, OpType::Message, Some([2u8; 32]), b"payload");
        assert_eq!(mine(u64::MAX), 1);

        // Re-enqueue of the same id is a silent no-op — still one row.
        enqueue(&id, OpType::Message, Some([2u8; 32]), b"payload");
        assert_eq!(mine(u64::MAX), 1);

        // Future backoff excludes the row from due-now.
        record_attempt(&id, u64::MAX);
        assert_eq!(mine(0), 0);

        // Dead rows never surface.
        mark_dead(&id);
        assert_eq!(mine(u64::MAX), 0);

        retire(&id);
        assert_eq!(mine(u64::MAX), 0);
    }
}
