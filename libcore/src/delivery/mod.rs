use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::Wake;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::PairDeclineP;
use common::proto::mls_wire::pair_decline_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::types::bytes::ByteVec;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use ed25519_dalek::SigningKey;
use log::debug;
use log::info;
use log::warn;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::params;

use crate::data::identity::Identity;
use crate::data::message::STATUS_FAILED;
use crate::data::message::STATUS_SENT;
use crate::db::outbox::OpType;
use crate::db::outbox::OutboxRow;
use crate::quic::dht_client::DhtClient;
use crate::quic::server::Session;
use crate::state::core;

/// Durability verdict for a dispatch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastOutcome {
    Durable,
    Reachable,
    Terminal,
    /// No ack came back; only the reconciler produces it.
    Silence,
}

/// Exhaustive on purpose: a new ack variant must be a compile error here, not a silent miscategory.
pub fn outcome_for_ack(ack: &DispatchAckP) -> LastOutcome {
    use LastOutcome::*;
    match ack {
        // `Queued` comes only after the relay's fsync, a durable handoff, so the message is sent
        // even while the recipient is offline.
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

// SQLite integers are i64: saturate so a `u64::MAX` sentinel stays `i64::MAX` instead of wrapping.
fn ms_i64(ms: u64) -> i64 {
    ms.min(i64::MAX as u64) as i64
}

pub fn enqueue(id: &[u8], op: OpType, target_ipk: Option<[u8; 32]>, payload: &[u8]) {
    if let Err(e) = enqueue_checked(id, op, target_ipk, payload) {
        warn!("OUTBOX: enqueue failed: {e}");
    }
}

fn enqueue_tx(conn: &Connection, id: &[u8], op: OpType, target: Option<[u8;32]>, payload:&[u8]) -> anyhow::Result<()> {
    conn.execute("INSERT INTO outbox(id,op_type,target_ipk,payload,created_at,next_attempt)
        VALUES (?1,?2,?3,?4,?5,0) ON CONFLICT(id,COALESCE(target_ipk,X'')) DO NOTHING",
        params![id,op as u8,target.as_ref().map(|p|p.as_slice()),payload,ms_i64(now_ms())])?;
    Ok(())
}

pub(crate) fn enqueue_checked(id:&[u8],op:OpType,target:Option<[u8;32]>,payload:&[u8])->anyhow::Result<()> {
    enqueue_tx(&core().db.outbox().lock(),id,op,target,payload)
}

pub(crate) fn enqueue_batch(copies:&mut [([u8;32],[u8;16],OpType,Vec<u8>)])->anyhow::Result<()> {
    enqueue_batch_in(&mut core().db.outbox().lock(), copies)
}

pub(crate) fn enqueue_batch_in(conn: &mut Connection, copies: &mut [([u8;32],[u8;16],OpType,Vec<u8>)]) -> anyhow::Result<()> {
    let tx=conn.transaction()?;
    for (to,id,op,bytes) in copies {
        enqueue_tx(&tx,id,*op,Some(*to),bytes)?;
        // A concurrent/replayed enqueue keeps the first envelope. Live sends
        // must use the same bytes the reconciler will replay.
        *bytes = tx.query_row("SELECT payload FROM outbox WHERE id=?1 AND target_ipk=?2",
            params![id.as_slice(),to.as_slice()],|r|r.get(0))?;
    }
    tx.commit()?;Ok(())
}

/// Retires one member's copy; each member acks on its own schedule.
pub fn retire(id: &[u8], target: Option<[u8; 32]>) {
    retire_tx(&core().db.outbox().lock(), id, target).ok();
}

pub(crate) fn retire_tx(
    conn: &Connection, id: &[u8], target: Option<[u8; 32]>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM outbox WHERE id = ?1 AND COALESCE(target_ipk, X'') = COALESCE(?2, X'')",
        params![id, target.as_ref().map(|t| t.as_slice())],
    )
}

pub fn retire_all(id: &[u8]) {
    retire_all_tx(&core().db.outbox().lock(), id).ok();
}

pub(crate) fn retire_all_tx(conn: &Connection, id: &[u8]) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])
}

/// Keeps pending-send recovery from rebuilding a dispatch the reconciler already owns.
pub fn any_pending(id: &[u8]) -> bool {
    any_pending_tx(&core().db.outbox().lock(), id).unwrap_or(false)
}

pub(crate) fn any_pending_tx(conn: &Connection, id: &[u8]) -> rusqlite::Result<bool> {
    conn.query_row("SELECT COUNT(*) FROM outbox WHERE id = ?1 AND state = 0", params![id], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n > 0)
}

pub fn forget_target(ipk: &[u8; 32]) {
    forget_target_tx(&core().db.outbox().lock(), ipk).ok();
}

pub(crate) fn forget_target_tx(conn: &Connection, ipk: &[u8; 32]) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM outbox WHERE target_ipk = ?1", params![ipk.as_slice()])
}

pub fn pending_ops_for(ipk: &[u8; 32]) -> u32 {
    pending_ops_for_tx(&core().db.outbox().lock(), ipk).unwrap_or(0)
}

pub(crate) fn pending_ops_for_tx(conn: &Connection, ipk: &[u8; 32]) -> rusqlite::Result<u32> {
    conn.query_row(
        "SELECT COUNT(*) FROM outbox WHERE target_ipk = ?1 AND state = 0",
        params![ipk.as_slice()],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u32)
}

pub(crate) fn due_tx(conn: &Connection, now_ms: u64) -> rusqlite::Result<Vec<OutboxRow>> {
    crate::db::all(
        conn,
        "SELECT * FROM outbox WHERE state = 0 AND next_attempt <= ?1 ORDER BY created_at ASC",
        params![ms_i64(now_ms)],
        OutboxRow::from_row,
    )
}

pub(crate) fn record_attempt_tx(
    conn: &Connection, id: &[u8], target: Option<[u8; 32]>, next_attempt: u64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE outbox SET attempts = attempts + 1, next_attempt = ?3 \
         WHERE id = ?1 AND COALESCE(target_ipk, X'') = COALESCE(?2, X'')",
        params![id, target.as_ref().map(|t| t.as_slice()), ms_i64(next_attempt)],
    )
}

pub(crate) fn mark_dead_tx(
    conn: &Connection, id: &[u8], target: Option<[u8; 32]>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE outbox SET state = 1 \
         WHERE id = ?1 AND COALESCE(target_ipk, X'') = COALESCE(?2, X'')",
        params![id, target.as_ref().map(|t| t.as_slice())],
    )
}

const BASE_BACKOFF_MS: u64 = 1_000;
const CAP_BACKOFF_MS: u64 = 300_000;
const DEAD_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000; // non-message silence past 7d dies
const MESSAGE_SILENCE_MAX: u32 = 6; // fail a message after this many no-ack retries (~2min)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    KeepRetrying,
    Retire,
    Dead,
}

/// Never fails a message prematurely, and never lets a persistently `Reachable` op die.
pub fn classify(op: OpType, last: LastOutcome, attempts: u32, age_ms: u64) -> Next {
    match last {
        LastOutcome::Durable | LastOutcome::Terminal => Next::Retire,
        LastOutcome::Reachable => Next::KeepRetrying, // a negative response still proves reachability
        // attempts-gated, not wall-clock, so an offline-queued msg isn't failed on reconnect
        LastOutcome::Silence => match op {
            OpType::Message if attempts >= MESSAGE_SILENCE_MAX => Next::Dead,
            _ if age_ms > DEAD_TTL_MS => Next::Dead,
            _ => Next::KeepRetrying,
        },
    }
}

// A plain `BASE << attempts` overflows u64, so cap the shift and the value.
fn next_backoff(attempts: u32) -> u64 {
    let shift = attempts.min(32); // 1000<<32 fits u64; caps well above CAP anyway
    (BASE_BACKOFF_MS << shift).min(CAP_BACKOFF_MS)
}

/// Re-dispatches every due row over `session`'s connection.
pub async fn reconcile(session: &Session) {
    reconcile_in(core().db.outbox(), session).await
}

/// One pass per session at a time: an overlapping pass would resend the same rows and count each
/// attempt twice.
async fn reconcile_in(outbox: &Mutex<Connection>, session: &Session) {
    let Ok(_pass) = session.reconciling.try_lock() else { return };
    let now = now_ms();
    let conn = &session.conn;

    let rows = due_tx(&outbox.lock(), now).unwrap_or_default();
    for row in rows {
        let op = OpType::from_u8(row.op_type).unwrap_or(OpType::Message);
        let target: Option<[u8; 32]> =
            row.target_ipk.as_ref().and_then(|t| t.as_slice().try_into().ok());
        let mut accepted_timestamp = None;
        let outcome = match op {
            OpType::KpPublish => {
                let Ok(recs) = Vec::<KeyPackageRecord>::deser(&row.payload) else {
                    // A poison payload can never publish.
                    retire_tx(&outbox.lock(), &row.id, target).ok();
                    continue;
                };
                match session.dht.publish_keypackages(&recs).await {
                    Ok(()) => LastOutcome::Durable,
                    // The relay answered but the DHT is not ready: keep retrying, never die.
                    Err(_) => LastOutcome::Reachable,
                }
            },
            // Replay the stored `.pack()`-framed bytes verbatim. Any transport error, or a reply
            // other than a `DispatchAck`, reads as Silence.
            _ => tokio::time::timeout(std::time::Duration::from_secs(15), async { match conn.open_bi().await {
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
            }}).await.unwrap_or(LastOutcome::Silence),
        };

        let age = now.saturating_sub(row.created_at);
        match classify(op, outcome, row.attempts, age) {
            Next::Retire => {
                if op == OpType::Message {
                    let status = if outcome == LastOutcome::Terminal { STATUS_FAILED } else { STATUS_SENT };
                    if let Err(e) = crate::data::receipts::send_result(&row.id, target, status, accepted_timestamp) {
                        warn!("MESSAGE: outcome persistence failed, retaining outbox: {e}");
                        continue;
                    }
                }
                retire_tx(&outbox.lock(), &row.id, target).ok();
            },
            Next::Dead => {
                if op == OpType::Message {
                    if let Err(e) = crate::data::receipts::send_result(&row.id, target, STATUS_FAILED, None) {
                        warn!("MESSAGE: failure persistence failed, retaining outbox: {e}");
                        continue;
                    }
                }
                mark_dead_tx(&outbox.lock(), &row.id, target).ok();
            },
            Next::KeepRetrying => {
                if matches!(op, OpType::Message) {
                    debug!("MESSAGE: {} still pending — {outcome:?} (attempt {})", hex::encode(&row.id[..row.id.len().min(4)]), row.attempts);
                }
                record_attempt_tx(
                    &outbox.lock(),
                    &row.id,
                    target,
                    now + next_backoff(row.attempts),
                )
                .ok();
            },
        }
    }
}

/// Signs and sends one dispatch of opaque bytes. With `outbox`, the framed bytes are persisted and
/// re-sent until a durable ack; `None` sends once.
pub(crate) async fn dispatch_envelope(
    session: Option<&Session>, to: [u8; 32], our_ipk: [u8; 32], ipk_signer: &SigningKey,
    env_bytes: Vec<u8>, wake: Wake, outbox: Option<OpType>,
) -> Result<()> {
    let id = crate::data::message::next_dispatch_id();
    let sig_message = dispatch_sig_message(common::PROTOCOL_VERSION, &to, &our_ipk, &id, &env_bytes);
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&sig_message).to_bytes()
    };
    let fwd = DispatchP {
        to:             Bytes(to),
        from:           Bytes(our_ipk),
        id:             Bytes(id),
        payload:        ByteVec(env_bytes),
        sig:            Bytes(sig),
        accepted_at_ms: 0,
        wake,
        ttl_ms:         0,
    };
    let bytes = CRelayPacket::Dispatch(fwd).pack().map_err(|e| anyhow!("pack dispatch: {e}"))?;
    if let Some(op) = outbox {
        enqueue(&id, op, Some(to), &bytes);
    }

    let Some(session) = session else {
        info!("MESSAGE: offline — dispatch to {} not sent", hex::encode(&to[..4]));
        bail!("offline");
    };
    let (mut tx, mut rx) =
        session.conn.open_bi().await.map_err(|e| anyhow!("open dispatch stream: {e}"))?;
    tx.write_all(&bytes).await.map_err(|e| anyhow!("write dispatch: {e}"))?;
    tx.finish().map_err(|e| anyhow!("finish dispatch: {e}"))?;
    let ack = match SRelayPacket::unpack(&mut rx).await {
        Ok(SRelayPacket::DispatchAck(ack)) => ack,
        Ok(other) => bail!("unexpected dispatch reply: {other:?}"),
        Err(e) => bail!("dispatch ack: {e}"),
    };
    if outcome_for_ack(&ack) != LastOutcome::Durable {
        bail!("relay did not accept dispatch: {ack:?}");
    }
    if outbox.is_some() {
        retire(&id, Some(to));
    }
    Ok(())
}

/// Signed with our IPK rather than sent through MLS, since accepting the group is what failed. The
/// inviter marks us rejected and fails the messages it sent while pending.
pub async fn send_pair_decline(to: [u8; 32], reason: u8) -> Result<()> {
    let our_ipk = Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?;
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;
    let ts = now_ms();
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&pair_decline_signing_input(&our_ipk, &to, reason, ts)).to_bytes()
    };
    let envelope = MlsEnvelopeP::PairDecline(PairDeclineP {
        sender_ipk: Bytes(our_ipk),
        recipient_ipk: Bytes(to),
        reason,
        timestamp: ts,
        sig: Bytes(sig),
    });
    let env_bytes = envelope.ser().map_err(|e| anyhow!("encode decline: {e}"))?;
    let session = core().session();
    dispatch_envelope(
        session.as_deref(),
        to,
        our_ipk,
        &ipk_signer,
        env_bytes,
        Wake::No,
        Some(OpType::Control),
    )
    .await
}

pub(crate) fn prepare_dispatch(
    to: &[u8; 32], our_ipk: &[u8; 32], ipk_signer: &SigningKey, id: &[u8; 16], payload: Vec<u8>,
    wake: Wake, ttl_ms: u64,
) -> Result<Vec<u8>> {
    let sig_message = dispatch_sig_message(common::PROTOCOL_VERSION, to, our_ipk, id, &payload);
    let sig = {
        use ed25519_dalek::Signer;
        ipk_signer.sign(&sig_message).to_bytes()
    };
    let fwd = DispatchP {
        to:             Bytes(*to),
        from:           Bytes(*our_ipk),
        id:             Bytes(*id),
        payload:        ByteVec(payload),
        sig:            Bytes(sig),
        accepted_at_ms: 0,
        wake,
        ttl_ms,
    };
    // `.pack()`, not `.ser()`: the relay reads length-prefixed frames. The outbox stores, sends and
    // replays these exact bytes.
    CRelayPacket::Dispatch(fwd).pack()
        .map_err(|e| anyhow!("frame dispatch: {e}"))
}

/// Enqueues and sends one member's copy. `Silence` covers every transport failure and leaves the
/// outbox row for the reconciler.
pub(crate) async fn dispatch_to_member(
    to: &[u8; 32], our_ipk: &[u8; 32], ipk_signer: &SigningKey, id: &[u8; 16], payload: Vec<u8>,
    op: OpType, wake: Wake, ttl_ms: u64,
) -> LastOutcome {
    let Ok(bytes) = prepare_dispatch(to, our_ipk, ipk_signer, id, payload, wake, ttl_ms) else {
        return LastOutcome::Terminal;
    };
    let mut copies = [(*to, *id, op, bytes)];
    if let Err(e) = enqueue_batch(&mut copies) {
        warn!("MESSAGE: could not queue dispatch: {e}");
        return LastOutcome::Silence;
    }
    dispatch_queued(core().session().as_deref(), to, id, op, &copies[0].3).await
}

/// Sends one queued copy over `session`; every failure leaves the outbox row to the reconciler.
pub(crate) async fn dispatch_queued(
    session: Option<&Session>, to: &[u8; 32], id: &[u8; 16], op: OpType, bytes: &[u8],
) -> LastOutcome {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let Some(session) = session else {
            info!("MESSAGE: offline — {} queued in outbox", hex::encode(&to[..4]));
            return LastOutcome::Silence;
        };
        let Ok((mut send, mut recv)) = session.conn.open_bi().await else {
            debug!("MESSAGE: {} send stream failed to open; left in outbox", hex::encode(&to[..4]));
            return LastOutcome::Silence;
        };
        if send.write_all(&bytes).await.is_err() || send.finish().is_err() {
            debug!("MESSAGE: {} interrupted mid-send; left in outbox", hex::encode(&to[..4]));
            return LastOutcome::Silence;
        }
        match SRelayPacket::unpack(&mut recv).await {
            Ok(SRelayPacket::DispatchAck(ack)) => {
                let outcome = outcome_for_ack(&ack);
                if matches!(outcome, LastOutcome::Durable | LastOutcome::Terminal) {
                    // Store the member outcome before retiring its durable outbox
                    // row. A crash or DB failure must not lose the only evidence.
                    if matches!(op, OpType::Message) {
                        let status = if matches!(outcome, LastOutcome::Durable) {
                            crate::data::message::STATUS_SENT
                        } else {
                            crate::data::message::STATUS_FAILED
                        };
                        if let Err(e) = crate::data::receipts::send_result(
                            id,
                            Some(*to),
                            status,
                            accepted_at_secs(&ack),
                        ) {
                            warn!("MESSAGE: receipt persistence failed, retaining outbox: {e}");
                            return LastOutcome::Silence;
                        }
                    }
                    retire(id, Some(*to));
                }
                outcome
            },
            _ => LastOutcome::Silence,
        }
    })
    .await;
    outcome.unwrap_or(LastOutcome::Silence)
}

#[cfg(test)]
mod tests {
    use common::proto::client_rel::CRelayPacket;
    use common::proto::client_rel::Wake;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::test_support::net;

    fn outbox() -> Mutex<Connection> {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::outbox::migrate(&mut conn);
        Mutex::new(conn)
    }

    /// A relay that answers proves a row can still go out, so only silence kills one: after a few
    /// attempts for a message, after a week for anything else.
    #[test]
    fn only_silence_kills_an_outbox_row() {
        use LastOutcome::*;
        use Next::*;
        use OpType::*;
        let acks = [
            (DispatchAckP::Queued { accepted_at_ms: 1 }, Durable),
            (DispatchAckP::Delivered { accepted_at_ms: 1 }, Durable),
            (DispatchAckP::Forwarded { accepted_at_ms: 1 }, Durable),
            (DispatchAckP::QueueFull, Reachable),
            (DispatchAckP::Error { reason: String::new() }, Reachable),
            (DispatchAckP::NotFound, Terminal),
            (DispatchAckP::InvalidSig, Terminal),
        ];
        for (ack, outcome) in acks {
            assert_eq!(outcome_for_ack(&ack), outcome, "{ack:?}");
        }

        let rows = [
            (Message, Durable, 0, 0, Retire),
            (Control, Terminal, 0, 0, Retire),
            (KpPublish, Reachable, u32::MAX, u64::MAX, KeepRetrying),
            (Message, Silence, MESSAGE_SILENCE_MAX - 1, 0, KeepRetrying),
            (Message, Silence, MESSAGE_SILENCE_MAX, 0, Dead),
            (Message, Silence, 0, DEAD_TTL_MS + 1, Dead),
            (Control, Silence, u32::MAX, DEAD_TTL_MS, KeepRetrying),
            (Welcome, Silence, 0, DEAD_TTL_MS + 1, Dead),
        ];
        for (op, last, attempts, age, next) in rows {
            assert_eq!(classify(op, last, attempts, age), next, "{op:?} {last:?} {attempts} {age}");
        }

        // `reconcile` reads each row's op back from its discriminant.
        for op in [Message, Welcome, KpPublish, Control] {
            assert_eq!(OpType::from_u8(op as u8), Some(op));
        }
        assert_eq!(OpType::from_u8(4), None);

        let cap = CAP_BACKOFF_MS;
        for (attempts, delay) in [
            (0, BASE_BACKOFF_MS),
            (1, 2_000),
            (8, 256_000),
            (9, cap),
            (31, cap),
            (32, cap),
            (33, cap),
            (u32::MAX, cap),
        ] {
            assert_eq!(next_backoff(attempts), delay, "attempt {attempts}");
        }
    }

    #[test]
    fn fanout_enqueue_is_atomic_and_replay_keeps_the_original_envelope() {
        let outbox = outbox();
        let mut conn = outbox.lock();
        conn.execute_batch(
            "CREATE TEMP TRIGGER refuse BEFORE INSERT ON outbox
             WHEN NEW.payload = CAST('bad' AS BLOB) BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        let (alice, bob, id) = ([2; 32], [3; 32], [1; 16]);
        let mut copies = [
            (alice, id, OpType::Message, b"first".to_vec()),
            (bob, id, OpType::Message, b"bad".to_vec()),
        ];
        assert!(enqueue_batch_in(&mut conn, &mut copies).is_err());
        assert!(due_tx(&conn, u64::MAX).unwrap().is_empty(), "no member's copy is queued alone");

        conn.execute_batch("DROP TRIGGER refuse;").unwrap();
        copies[1].3 = b"first".to_vec();
        enqueue_batch_in(&mut conn, &mut copies).unwrap();
        // A replay seals fresh ciphertext, but the live send must reuse what the outbox holds.
        copies[0].3 = b"retry".to_vec();
        enqueue_batch_in(&mut conn, &mut copies).unwrap();
        assert_eq!(copies[0].3, b"first");
        let rows = due_tx(&conn, u64::MAX).unwrap();
        assert_eq!(rows.len(), 2, "one row per member");
        assert!(rows.iter().all(|r| r.payload == b"first"));

        retire_tx(&conn, &id, Some(alice)).unwrap();
        assert!(any_pending_tx(&conn, &id).unwrap(), "each member acknowledges on its own");
        retire_tx(&conn, &id, Some(bob)).unwrap();
        assert!(!any_pending_tx(&conn, &id).unwrap());
    }

    /// The timer and a reconnect both start passes; while one waits on the relay, another must
    /// neither resend its rows nor count a second attempt against them.
    #[tokio::test(start_paused = true)]
    async fn overlapping_passes_send_a_row_once_and_count_one_attempt() {
        let _clock = net::step_paused_clock();
        let (ours, relay) = net::connection().await;
        let session = net::session(ours);
        let outbox = outbox();
        let signer = SigningKey::from_bytes(&[7; 32]);
        let (peer, id) = ([8; 32], [9; 16]);
        let frame = prepare_dispatch(
            &peer,
            &signer.verifying_key().to_bytes(),
            &signer,
            &id,
            b"control".to_vec(),
            Wake::No,
            0,
        )
        .unwrap();
        enqueue_tx(&outbox.lock(), &id, OpType::Control, Some(peer), &frame).unwrap();

        // Reads every dispatch and acknowledges none.
        let relay = tokio::spawn(async move {
            let (mut ids, mut unanswered) = (Vec::new(), Vec::new());
            while let Ok((send, mut recv)) = relay.accept_bi().await {
                if let Ok(CRelayPacket::Dispatch(dispatch)) = CRelayPacket::unpack(&mut recv).await
                {
                    ids.push(dispatch.id.0);
                }
                unanswered.push(send);
            }
            ids
        });
        tokio::join!(reconcile_in(&outbox, &session), reconcile_in(&outbox, &session));
        session.conn.close(0u32.into(), b"done");
        assert_eq!(relay.await.unwrap(), [id]);
        assert_eq!(due_tx(&outbox.lock(), u64::MAX).unwrap()[0].attempts, 1);
    }
}
