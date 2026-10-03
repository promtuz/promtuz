//! Recipient-scoped delivery history. A relay acknowledgement is sent, never
//! delivered/read. Original audiences survive roster changes and partial failure.
use super::message::{STATUS_DELIVERED, STATUS_FAILED, STATUS_PENDING, STATUS_READ, STATUS_SENT};
use crate::db::all;
use crate::state::core;
use anyhow::{Result, ensure};
use common::proto::mls_wire::{AppPayload, ReceiptDetails, ReceiptEntry, ReceiptKind};
use common::utils::now_secs;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, uniffi::Record)]
pub struct RecipientReceipt {
    pub member: Vec<u8>,
    pub name: String,
    pub active: bool,
    pub status: u8,
    pub sent_at: Option<u64>,
    pub delivered_at: Option<u64>,
    pub read_at: Option<u64>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageReceiptInfo {
    pub complete: bool,
    pub recipients: Vec<RecipientReceipt>,
}

// Detailed peers acknowledge exact IDs. Legacy watermarks remain a compatibility
// fallback for older peers; they carry no event time and must never invent one.
const ROWS: &str = "SELECT r.member,r.sent_at,r.delivered_at,r.read_at,
 CASE WHEN r.read_at IS NOT NULL THEN 4 WHEN r.delivered_at IS NOT NULL THEN 3
 WHEN NOT EXISTS(SELECT 1 FROM receipt_peers p WHERE p.conversation_id=m.conversation_id AND p.member=r.member)
 THEN MAX(r.send_status,r.legacy_status) ELSE r.send_status END AS status,
 COALESCE((SELECT active FROM conversation_members c WHERE c.conversation_id=m.conversation_id AND c.member_ipk=r.member),0) AS active
 FROM message_recipients r JOIN messages m ON m.id=r.message_id WHERE r.message_id=?1";

pub(super) fn snapshot_tx(
    conn: &Connection, id: &str, recipients: &[[u8; 32]], complete: bool,
) -> Result<()> {
    if conn.execute(
        "INSERT OR IGNORE INTO message_audiences(message_id,complete) VALUES (?1,?2)",
        (id, complete),
    )? == 0
    {
        return Ok(());
    }
    for peer in recipients {
        conn.execute(
            "INSERT INTO message_recipients(message_id,member) VALUES (?1,?2)",
            (id, peer.as_slice()),
        )?;
    }
    Ok(())
}

pub(crate) fn audience(id: &str, fallback: &[[u8; 32]]) -> Result<Vec<[u8; 32]>> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    let peers = audience_tx(&tx, id, fallback)?;
    tx.commit()?;
    Ok(peers)
}

pub(crate) fn audience_tx(conn: &Connection, id: &str, fallback: &[[u8; 32]]) -> Result<Vec<[u8; 32]>> {
    // An older message has no provable full audience. Preserve that limitation.
    snapshot_tx(conn, id, fallback, false)?;
    Ok(all(conn, "SELECT member FROM message_recipients WHERE message_id=?1", [id], |r| r.get(0))?)
}

fn rows_tx(conn: &Connection, id: &str) -> Result<Vec<RecipientReceipt>> {
    Ok(all(conn, ROWS, [id], |r| {
        Ok(RecipientReceipt {
            member: r.get(0)?,
            name: String::new(),
            sent_at: r.get(1)?,
            delivered_at: r.get(2)?,
            read_at: r.get(3)?,
            status: r.get(4)?,
            active: r.get(5)?,
        })
    })?)
}

/// Combined bubble/album progress. Failure does not erase a successful copy.
pub(crate) fn combined_status(statuses: &[u8], complete: bool) -> u8 {
    if statuses.is_empty() {
        return STATUS_PENDING;
    }
    if complete && statuses.iter().all(|s| *s == STATUS_READ) {
        STATUS_READ
    } else if complete && statuses.iter().all(|s| *s >= STATUS_DELIVERED) {
        STATUS_DELIVERED
    } else if statuses.iter().any(|s| *s == STATUS_SENT || *s >= STATUS_DELIVERED) {
        STATUS_SENT
    } else if statuses.contains(&STATUS_PENDING) {
        STATUS_PENDING
    } else {
        STATUS_FAILED
    }
}

fn aggregate_tx(conn: &Connection, id: &str) -> Result<()> {
    let rows = rows_tx(conn, id)?;
    if rows.is_empty() {
        return Ok(());
    }
    let complete: bool =
        conn.query_row("SELECT complete FROM message_audiences WHERE message_id=?1", [id], |r| {
            r.get(0)
        })?;
    let status = combined_status(&rows.iter().map(|r| r.status).collect::<Vec<_>>(), complete);
    conn.execute(
        "UPDATE messages SET status=?2 WHERE id=?1 AND status<>?2 AND (?3 OR status<3)",
        (id, status, complete),
    )?;
    Ok(())
}

pub(crate) fn send_result(
    dispatch: &[u8], member: Option<[u8; 32]>, status: u8, at: Option<u64>,
) -> Result<()> {
    let Some(member) = member else { return Ok(()) };
    let dispatch = crate::mls::recovery::logical_dispatch(dispatch)?;
    let at = at.filter(|time| *time > 0 && *time <= i64::MAX as u64);
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    send_result_tx(&tx, &dispatch, &member, status, at)?;
    tx.commit()?;
    Ok(())
}

fn send_result_tx(
    conn: &Connection, dispatch: &[u8], member: &[u8; 32], status: u8, at: Option<u64>,
) -> Result<()> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM messages WHERE dispatch_id=?1 AND outgoing=1 AND deleted=0",
            [dispatch],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(()) };
    // Recover old queued sends individually without pretending their full
    // historical roster or other retired outcomes can be reconstructed.
    conn.execute(
        "INSERT OR IGNORE INTO message_audiences(message_id,complete) VALUES (?1,0)",
        [&id],
    )?;
    conn.execute("INSERT OR IGNORE INTO message_recipients(message_id,member)
        SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM message_audiences WHERE message_id=?1 AND complete=0)",(&id,member.as_slice()))?;
    conn.execute(
        "UPDATE message_recipients SET send_status=CASE WHEN send_status=1 THEN 1 ELSE ?3 END,
        sent_at=COALESCE(sent_at,?4) WHERE message_id=?1 AND member=?2",
        params![id, member.as_slice(), status, at],
    )?;
    aggregate_tx(conn, &id)
}

pub(crate) fn fail_pending(id: &str) -> Result<()> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    fail_pending_tx(&tx, id)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn fail_pending_tx(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE message_recipients SET send_status=2 WHERE message_id=?1 AND send_status=0",
        [id],
    )?;
    if conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_recipients WHERE message_id=?1)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        aggregate_tx(conn, id)?;
    } else {
        conn.execute("UPDATE messages SET status=2 WHERE id=?1 AND status<3", [id])?;
    }
    Ok(())
}

pub fn info(conv: &[u8; 16], dispatch: &[u8; 16]) -> Result<MessageReceiptInfo> {
    let MessageReceiptInfo { complete, mut recipients } =
        info_tx(&core().db.messages().lock(), conv, dispatch)?;
    for row in &mut recipients {
        row.name = super::peer_name::resolve(&row.member.as_slice().try_into()?);
    }
    recipients.sort_by(|a, b| {
        b.status
            .cmp(&a.status)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.member.cmp(&b.member))
    });
    Ok(MessageReceiptInfo { complete, recipients })
}

/// Unnamed and unsorted: names come from a lookup that takes this connection's lock.
pub(crate) fn info_tx(
    conn: &Connection, conv: &[u8; 16], dispatch: &[u8; 16],
) -> Result<MessageReceiptInfo> {
    let id:String=conn.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=1 AND deleted=0",(conv.as_slice(),dispatch.as_slice()),|r|r.get(0))?;
    let complete = conn
        .query_row("SELECT complete FROM message_audiences WHERE message_id=?1", [&id], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(false);
    Ok(MessageReceiptInfo { complete, recipients: rows_tx(conn, &id)? })
}

pub(crate) fn receive(conv: &[u8; 16], member: &[u8; 32], payload: AppPayload) -> Result<()> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    receive_tx(&tx, conv, member, payload)?;
    tx.commit()?;
    Ok(())
}

fn receive_tx(
    conn: &Connection, conv: &[u8; 16], member: &[u8; 32], payload: AppPayload,
) -> Result<()> {
    let mut changed = Vec::<String>::new();
    match payload {
        AppPayload::ReceiptDetails(details) => {
            ensure!(
                !details.entries.is_empty() && details.entries.len() <= 128,
                "invalid receipt batch"
            );
            for entry in &details.entries {
                ensure!(
                    entry.delivered_at.is_some() || entry.read_at.is_some(),
                    "receipt has no event"
                );
                ensure!(
                    [entry.delivered_at, entry.read_at]
                        .into_iter()
                        .flatten()
                        .all(|n| n > 0 && n <= i64::MAX as u64),
                    "invalid receipt time"
                );
            }
            // An authenticated group member can speak only for their own seat.
            let eligible:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM conversation_members WHERE conversation_id=?1 AND member_ipk=?2)",
                (conv.as_slice(),member.as_slice()),|r|r.get(0))?;
            if !eligible {
                return Ok(());
            }
            let activated = conn.execute(
                "INSERT OR IGNORE INTO receipt_peers VALUES (?1,?2)",
                (conv.as_slice(), member.as_slice()),
            )? == 1;
            for entry in details.entries {
                let id:Option<String>=conn.query_row("SELECT r.message_id FROM message_recipients r JOIN messages m ON m.id=r.message_id
                    WHERE m.conversation_id=?1 AND m.dispatch_id=?2 AND m.outgoing=1 AND r.member=?3",
                    (conv.as_slice(),entry.message_id.as_slice(),member.as_slice()),|r|r.get(0)).optional()?;
                let Some(id) = id else { continue };
                // Earliest reported event is stable across duplicates and reversed
                // arrival. Read implies delivery, but never invent its exact time.
                conn.execute("UPDATE message_recipients SET delivered_at=CASE WHEN delivered_at IS NULL THEN ?3 WHEN ?3 IS NULL THEN delivered_at ELSE MIN(delivered_at,?3) END,
                    read_at=CASE WHEN read_at IS NULL THEN ?4 WHEN ?4 IS NULL THEN read_at ELSE MIN(read_at,?4) END
                    WHERE message_id=?1 AND member=?2",params![id,member.as_slice(),entry.delivered_at,entry.read_at])?;
                changed.push(id);
            }
            // The first exact receipt from a member retires every provisional
            // legacy inference for them, skipped IDs included.
            if activated {
                changed=all(conn,"SELECT r.message_id FROM message_recipients r JOIN messages m ON m.id=r.message_id WHERE m.conversation_id=?1 AND r.member=?2",
                    (conv.as_slice(),member.as_slice()),|r|r.get(0))?;
            }
        },
        AppPayload::Receipt { kind, upto } => {
            // A member that sends exact receipts has no use for watermarks.
            if detailed_tx(conn, conv, member)? {
                return Ok(());
            }
            // Old clients broadcast watermarks for every author's IDs. Require
            // the named target to be OUR post sent to this reporting member.
            let known:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN message_recipients r ON r.message_id=m.id
                WHERE m.conversation_id=?1 AND m.dispatch_id=?2 AND m.outgoing=1 AND r.member=?3)",
                (conv.as_slice(),upto.as_slice(),member.as_slice()),|r|r.get(0))?;
            if !known {
                return Ok(());
            }
            let status = match kind {
                ReceiptKind::Delivered => STATUS_DELIVERED,
                ReceiptKind::Read => STATUS_READ,
            };
            changed=all(conn,"SELECT r.message_id FROM message_recipients r JOIN messages m ON m.id=r.message_id
                WHERE m.conversation_id=?1 AND m.dispatch_id<=?2 AND m.outgoing=1 AND r.member=?3 AND r.legacy_status<?4",
                (conv.as_slice(),upto.as_slice(),member.as_slice(),status),|r|r.get(0))?;
            for id in &changed {
                conn.execute("UPDATE message_recipients SET legacy_status=MAX(legacy_status,?3) WHERE message_id=?1 AND member=?2",(id,member.as_slice(),status))?;
            }
        },
        _ => {},
    }
    for id in changed {
        aggregate_tx(conn, &id)?;
    }
    Ok(())
}

/// Whether `member` has sent exact receipts in `conv`.
fn detailed_tx(conn: &Connection, conv: &[u8; 16], member: &[u8; 32]) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM receipt_peers WHERE conversation_id=?1 AND member=?2)",
        (conv.as_slice(), member.as_slice()),
        |r| r.get(0),
    )?)
}

/// Every incoming message gets its receipt row, read according to the legacy
/// watermark where it still has to stand in for one. Idempotent.
pub(crate) fn backfill_tx(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT OR IGNORE INTO incoming_receipts(message_id,is_read)
        SELECT m.id, CASE WHEN r.upto_dispatch_id IS NOT NULL AND m.dispatch_id<=r.upto_dispatch_id THEN 1 ELSE 0 END
        FROM messages m LEFT JOIN read_state r ON r.conversation_id=m.conversation_id
        WHERE m.outgoing=0 AND m.system=0 AND m.dispatch_id IS NOT NULL",
    )?;
    Ok(())
}

pub(super) fn arrived_tx(conn: &Connection, id: &str, now: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO incoming_receipts(message_id,delivered_at,pending) VALUES (?1,?2,1)",
        (id, now),
    )?;
    Ok(())
}

pub(crate) fn read(conv: &[u8; 16], upto: &[u8; 16]) -> Result<()> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    read_tx(&tx, conv, upto, now_secs())?;
    tx.commit()?;
    Ok(())
}

pub(super) fn read_tx(conn: &Connection, conv: &[u8; 16], upto: &[u8; 16], now: u64) -> Result<()> {
    // The UI anchor is arrival order, not a comparison between different
    // authors' clocks. Only messages actually present on this device are read.
    conn.execute("UPDATE incoming_receipts SET is_read=1,read_at=?3,pending=1 WHERE is_read=0 AND message_id IN
        (SELECT id FROM messages WHERE conversation_id=?1 AND outgoing=0 AND deleted=0 AND system=0
         AND id<=(SELECT id FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=0))",(conv.as_slice(),upto.as_slice(),now))?;
    // Keep the old projection for older backup readers. New unread counts use
    // the exact local ledger so late arrivals cannot become automatically read.
    conn.execute("INSERT INTO read_state VALUES (?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET upto_dispatch_id=MAX(upto_dispatch_id,excluded.upto_dispatch_id)",
        (conv.as_slice(),upto.as_slice()))?;
    Ok(())
}

#[derive(Clone)]
struct Pending {
    id: String,
    conv: [u8; 16],
    author: [u8; 32],
    entry: ReceiptEntry,
}
fn pending(skip: &std::collections::HashSet<([u8; 16], [u8; 32])>) -> Result<Vec<Pending>> {
    let groups = pending_groups_tx(&core().db.messages().lock())?;
    // A request's receipts wait until it is accepted.
    let first = groups.into_iter().find(|group| !skip.contains(group) && !crate::requests::is_request_chat(&group.0));
    let Some((conv, author)) = first else { return Ok(Vec::new()) };
    pending_rows_tx(&core().db.messages().lock(), conv, author)
}

/// Each (conversation, author) with receipts waiting, oldest first.
fn pending_groups_tx(conn: &Connection) -> Result<Vec<([u8; 16], [u8; 32])>> {
    Ok(all(conn, "SELECT m.conversation_id,m.sender_ipk FROM incoming_receipts r CROSS JOIN messages m ON m.id=r.message_id
        JOIN conversations c ON c.id=m.conversation_id WHERE r.pending=1 AND m.deleted=0 AND c.mls_group_id IS NOT NULL
        AND EXISTS(SELECT 1 FROM conversation_members cm WHERE cm.conversation_id=m.conversation_id AND cm.member_ipk=m.sender_ipk AND cm.active=1)
        GROUP BY m.conversation_id,m.sender_ipk ORDER BY MIN(m.id)",
        [],|r|Ok((r.get(0)?,r.get(1)?)))?)
}

fn pending_rows_tx(conn: &Connection, conv: [u8; 16], author: [u8; 32]) -> Result<Vec<Pending>> {
    Ok(all(conn, "SELECT m.id,m.dispatch_id,r.delivered_at,r.read_at FROM incoming_receipts r CROSS JOIN messages m ON m.id=r.message_id
        WHERE r.pending=1 AND m.deleted=0 AND m.conversation_id=?1 AND m.sender_ipk=?2 ORDER BY m.id LIMIT 128",
        (conv.as_slice(),author.as_slice()),|r|Ok(Pending {id:r.get(0)?,conv,author,
            entry:ReceiptEntry {message_id:r.get(1)?,delivered_at:r.get(2)?,read_at:r.get(3)?}}))?)
}

static FLUSH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn schedule() {
    use std::sync::atomic::Ordering::SeqCst;
    REQUESTED.store(true, SeqCst);
    if RUNNING.swap(true, SeqCst) {
        return;
    }
    core().spawn(async {
        while REQUESTED.swap(false, SeqCst) {
            if let Err(e) = flush().await {
                log::debug!("RECEIPTS: deferred: {e}");
                break;
            }
        }
        RUNNING.store(false, SeqCst);
        if REQUESTED.load(SeqCst) {
            schedule();
        }
    });
}
pub(crate) async fn flush() -> Result<()> {
    let _guard = FLUSH.lock().await;
    let mut failed = std::collections::HashSet::new();
    loop {
        let rows = pending(&failed)?;
        let Some(first) = rows.first() else { return Ok(()) };
        if let Err(e) = send_batch(&rows).await {
            log::debug!("RECEIPTS: chat deferred: {e}");
            failed.insert((first.conv, first.author));
            continue;
        }
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        finish_batch_tx(&tx, &rows)?;
        tx.commit()?;
    }
}

/// Clears only rows unchanged since the batch was loaded, so a newer event still goes out.
fn finish_batch_tx(conn: &Connection, rows: &[Pending]) -> Result<()> {
    for row in rows {
        conn.execute("UPDATE incoming_receipts SET pending=0 WHERE message_id=?1 AND delivered_at IS ?2 AND read_at IS ?3",
            params![row.id,row.entry.delivered_at,row.entry.read_at])?;
    }
    Ok(())
}

async fn send_batch(rows: &[Pending]) -> Result<()> {
    let first = &rows[0];
    // Persisted event times survive offline encryption failure/restart.
    // Each control is itself outboxed before awaiting the relay.
    crate::messaging::send_control_to(
        first.conv,
        AppPayload::ReceiptDetails(ReceiptDetails {
            entries: rows.iter().map(|r| r.entry.clone()).collect(),
        }),
        first.author,
    )
    .await?;
    // Watermarks only for an author that has never sent exact receipts, and
    // only to them, never the whole group.
    if detailed_tx(&core().db.messages().lock(), &first.conv, &first.author)? {
        return Ok(());
    }
    for kind in [ReceiptKind::Delivered, ReceiptKind::Read] {
        let upto = rows
            .iter()
            .filter(|r| match kind {
                ReceiptKind::Delivered => {
                    r.entry.delivered_at.is_some() || r.entry.read_at.is_some()
                },
                ReceiptKind::Read => r.entry.read_at.is_some(),
            })
            .map(|r| r.entry.message_id)
            .max();
        if let Some(upto) = upto {
            crate::messaging::send_control_to(
                first.conv,
                AppPayload::Receipt { kind, upto },
                first.author,
            )
            .await?;
        }
    }
    Ok(())
}

/// Albums are several stored messages presented as one bubble. Report each
/// recipient's progress across the parts they were actually addressed.
pub fn info_many(conv: &[u8; 16], dispatches: &[[u8; 16]]) -> Result<MessageReceiptInfo> {
    ensure!(!dispatches.is_empty() && dispatches.len() <= 128, "invalid message selection");
    let mut complete = true;
    let mut people = std::collections::BTreeMap::<Vec<u8>, Vec<RecipientReceipt>>::new();
    for did in dispatches {
        let info = info(conv, did)?;
        complete &= info.complete;
        for row in info.recipients {
            people.entry(row.member.clone()).or_default().push(row);
        }
    }
    let mut recipients = Vec::new();
    for (_, parts) in people {
        let mut row = parts[0].clone();
        row.status = combined_status(&parts.iter().map(|r| r.status).collect::<Vec<_>>(), true);
        row.sent_at = parts
            .iter()
            .map(|p| p.sent_at)
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().max());
        row.delivered_at = parts
            .iter()
            .map(|p| p.delivered_at)
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().max());
        row.read_at = parts
            .iter()
            .map(|p| p.read_at)
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().max());
        recipients.push(row);
    }
    recipients.sort_by(|a, b| {
        b.status.cmp(&a.status).then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(MessageReceiptInfo { complete, recipients })
}

type SavedRecipient = (String, [u8; 32], u8, Option<u64>, Option<u64>, Option<u64>, u8);
type SavedIncoming = (String, Option<u64>, Option<u64>, bool, bool);
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Backup {
    audiences: Vec<(String, bool)>,
    recipients: Vec<SavedRecipient>,
    incoming: Vec<SavedIncoming>,
    peers: Vec<([u8; 16], [u8; 32])>,
}

pub(crate) fn dump_tx(conn: &Connection) -> Result<Backup> {
    Ok(Backup {
        audiences:all(conn,"SELECT message_id,complete FROM message_audiences",[],|r|Ok((r.get(0)?,r.get(1)?)))?,
        recipients:all(conn,"SELECT message_id,member,send_status,sent_at,delivered_at,read_at,legacy_status FROM message_recipients",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?,
        incoming:all(conn,"SELECT message_id,delivered_at,read_at,is_read,pending FROM incoming_receipts",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?,
        peers:all(conn,"SELECT conversation_id,member FROM receipt_peers",[],|r|Ok((r.get(0)?,r.get(1)?)))?,
    })
}

pub(crate) fn restore_tx(conn: &Connection, backup: &Backup) -> Result<()> {
    let mut new = std::collections::HashSet::new();
    for (id, complete) in &backup.audiences {
        if conn.execute(
            "INSERT OR IGNORE INTO message_audiences(message_id,complete)
            SELECT id,?2 FROM messages WHERE id=?1 AND outgoing=1",
            (id, complete),
        )? > 0
        {
            new.insert(id);
        }
    }
    for (id, peer, status, sent, delivered, read, legacy) in &backup.recipients {
        if new.contains(id) {
            conn.execute(
                "INSERT OR IGNORE INTO message_recipients(message_id,member) VALUES (?1,?2)",
                (id, peer.as_slice()),
            )?;
        }
        // Merge only known audience seats; a backup cannot broaden a live one.
        conn.execute("UPDATE message_recipients SET send_status=CASE WHEN send_status=1 OR ?3=1 THEN 1 ELSE MAX(send_status,?3) END,
            sent_at=COALESCE(sent_at,?4),delivered_at=COALESCE(delivered_at,?5),read_at=COALESCE(read_at,?6),legacy_status=MAX(legacy_status,?7)
            WHERE message_id=?1 AND member=?2",params![id,peer.as_slice(),status,sent,delivered,read,legacy])?;
    }
    for (id, delivered, read, is_read, pending) in &backup.incoming {
        conn.execute("INSERT OR IGNORE INTO incoming_receipts(message_id) SELECT id FROM messages WHERE id=?1 AND outgoing=0",[id])?;
        conn.execute("UPDATE incoming_receipts SET delivered_at=COALESCE(delivered_at,?2),read_at=COALESCE(read_at,?3),is_read=MAX(is_read,?4),pending=MAX(pending,?5) WHERE message_id=?1",
            params![id,delivered,read,is_read,pending])?;
    }
    for (conv, member) in &backup.peers {
        conn.execute("INSERT OR IGNORE INTO receipt_peers SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM conversations WHERE id=?1)",(conv.as_slice(),member.as_slice()))?;
    }
    for (id, _) in &backup.audiences {
        aggregate_tx(conn, id)?;
    }
    Ok(())
}

pub(crate) fn reject_conversation(conv: &[u8; 16]) -> Result<()> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    reject_conversation_tx(&tx, conv)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn reject_conversation_tx(conn: &Connection, conv: &[u8; 16]) -> Result<()> {
    let ids: Vec<String> = all(
        conn,
        "SELECT id FROM messages WHERE conversation_id=?1 AND outgoing=1 AND status<3",
        [conv.as_slice()],
        |r| r.get(0),
    )?;
    for id in ids {
        conn.execute("UPDATE message_recipients SET send_status=2 WHERE message_id=?1", [&id])?;
        conn.execute("UPDATE messages SET status=2 WHERE id=?1", [&id])?;
        aggregate_tx(conn, &id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::conversation::Conversation;
    use crate::data::conversation::ROLE_MEMBER;
    use crate::data::message::Message;
    use crate::test_support::data::open;
    use crate::test_support::data::ulid;
    use crate::test_support::data::with_failing_trigger;

    const A: [u8; 32] = [2; 32];
    const B: [u8; 32] = [3; 32];

    /// A chat of A and B on a device of ours that is not in the roster.
    fn db() -> (Connection, [u8; 16]) {
        let conn = open(crate::db::messages::migrate);
        let conv = Conversation::join_group_tx(&conn, &A, &[A, B]).unwrap();
        (conn, conv)
    }

    /// Our post, its audience frozen at A and B.
    fn post(conn: &Connection, conv: [u8; 16]) -> (String, [u8; 16]) {
        let m = Message::save_outgoing_tx(conn, conv, "hello", None, None).unwrap().inner;
        (m.id.to_string(), m.dispatch_id.unwrap().try_into().unwrap())
    }

    fn state(conn: &Connection, id: &str) -> u8 {
        conn.query_row("SELECT status FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    fn details(did: [u8; 16], delivered_at: Option<u64>, read_at: Option<u64>) -> AppPayload {
        AppPayload::ReceiptDetails(ReceiptDetails {
            entries: vec![ReceiptEntry { message_id: did, delivered_at, read_at }],
        })
    }

    fn exact(
        conn: &Connection, conv: [u8; 16], peer: &[u8; 32], did: [u8; 16], d: Option<u64>,
        r: Option<u64>,
    ) {
        receive_tx(conn, &conv, peer, details(did, d, r)).unwrap();
    }

    #[test]
    fn mixed_send_results_are_order_independent_and_do_not_downgrade_receipts() {
        for reversed in [false, true] {
            let (conn, conv) = db();
            let (id, did) = post(&conn, conv);
            let mut results = [(A, STATUS_SENT, Some(10)), (B, STATUS_FAILED, None)];
            if reversed {
                results.reverse();
            }
            for (peer, status, at) in results {
                send_result_tx(&conn, &did, &peer, status, at).unwrap();
            }
            assert_eq!(state(&conn, &id), STATUS_SENT);
            let failed =
                rows_tx(&conn, &id).unwrap().iter().filter(|r| r.status == STATUS_FAILED).count();
            assert_eq!(failed, 1);
            exact(&conn, conv, &A, did, Some(11), Some(12));
            exact(&conn, conv, &B, did, Some(13), None);
            assert_eq!(
                state(&conn, &id),
                STATUS_DELIVERED,
                "one read and one delivered is not all read"
            );
            exact(&conn, conv, &B, did, None, Some(14));
            send_result_tx(&conn, &did, &A, STATUS_FAILED, None).unwrap();
            send_result_tx(&conn, &did, &B, STATUS_SENT, Some(20)).unwrap();
            assert_eq!(state(&conn, &id), STATUS_READ, "a late send result never undoes a read");
            exact(&conn, conv, &B, did, Some(19), Some(21));
            let rows = rows_tx(&conn, &id).unwrap();
            let b = rows.iter().find(|r| r.member == B).unwrap();
            assert_eq!(
                (b.delivered_at, b.read_at),
                (Some(13), Some(14)),
                "the first report stands"
            );
        }
        let (conn, conv) = db();
        let (id, did) = post(&conn, conv);
        send_result_tx(&conn, &did, &A, STATUS_FAILED, None).unwrap();
        assert_eq!(state(&conn, &id), STATUS_PENDING);
        send_result_tx(&conn, &did, &B, STATUS_FAILED, None).unwrap();
        assert_eq!(state(&conn, &id), STATUS_FAILED);
    }

    #[test]
    fn exact_ids_ignore_legacy_holes_and_watermarks_from_another_author() {
        let (conn, conv) = db();
        let (first, a) = post(&conn, conv);
        let (second, b) = post(&conn, conv);
        for did in [a, b] {
            for peer in [A, B] {
                send_result_tx(&conn, &did, &peer, STATUS_SENT, Some(10)).unwrap();
            }
        }
        let legacy = |peer: &[u8; 32], kind, upto| {
            receive_tx(&conn, &conv, peer, AppPayload::Receipt { kind, upto }).unwrap();
        };
        // A reordered legacy watermark can precede exact support; once exact evidence arrives,
        // the post it skipped is no longer inferred read.
        legacy(&A, ReceiptKind::Read, b);
        exact(&conn, conv, &A, b, Some(11), Some(12));
        assert_eq!(rows_tx(&conn, &first).unwrap()[0].status, STATUS_SENT);
        assert_eq!(rows_tx(&conn, &second).unwrap()[0].status, STATUS_READ);
        legacy(&A, ReceiptKind::Read, b);
        assert_eq!(rows_tx(&conn, &first).unwrap()[0].status, STATUS_SENT);
        legacy(&B, ReceiptKind::Read, [0xFF; 16]);
        assert_eq!(
            rows_tx(&conn, &second).unwrap()[1].status,
            STATUS_SENT,
            "a watermark naming another author's post cannot mark ours"
        );
        legacy(&B, ReceiptKind::Delivered, b);
        assert_eq!(state(&conn, &second), STATUS_DELIVERED);
        assert_eq!(
            rows_tx(&conn, &second).unwrap()[1].delivered_at,
            None,
            "legacy receipts carry no time"
        );
    }

    #[test]
    fn original_audience_survives_joins_leaves_and_foreign_receipts() {
        let (conn, conv) = db();
        let (id, did) = post(&conn, conv);
        let joiner = [4; 32];
        Conversation::put_member(&conn, &conv, &joiner, ROLE_MEMBER).unwrap();
        snapshot_tx(&conn, &id, &[A, B, joiner], true).unwrap();
        exact(&conn, conv, &joiner, did, Some(11), Some(12));
        assert_eq!(rows_tx(&conn, &id).unwrap().len(), 2, "a joiner never enters the audience");

        Conversation::deactivate_member_tx(&conn, &conv, &B).unwrap();
        exact(&conn, conv, &A, did, Some(11), Some(12));
        assert_eq!(state(&conn, &id), STATUS_SENT);
        assert!(!rows_tx(&conn, &id).unwrap()[1].active);
        exact(&conn, conv, &B, did, Some(13), Some(14));
        assert_eq!(
            state(&conn, &id),
            STATUS_READ,
            "a departed recipient's late receipt still counts"
        );

        let before = rows_tx(&conn, &id).unwrap()[0].read_at;
        receive_tx(&conn, &[8; 16], &A, details(did, None, Some(1))).unwrap();
        assert_eq!(
            rows_tx(&conn, &id).unwrap()[0].read_at,
            before,
            "a receipt from another chat is ignored"
        );
    }

    #[test]
    fn a_failed_receipt_transaction_publishes_nothing() {
        let (mut conn, conv) = db();
        let (id, did) = post(&conn, conv);
        with_failing_trigger(&mut conn, "message_recipients", "UPDATE", |conn| {
            let tx = conn.transaction().unwrap();
            assert!(receive_tx(&tx, &conv, &A, details(did, Some(11), None)).is_err());
        });
        let peers: u32 =
            conn.query_row("SELECT COUNT(*) FROM receipt_peers", [], |r| r.get(0)).unwrap();
        assert_eq!(peers, 0, "the member was not marked as sending exact receipts");
        assert_eq!(state(&conn, &id), STATUS_PENDING);

        let tx = conn.transaction().unwrap();
        exact(&tx, conv, &A, did, Some(11), None);
        tx.commit().unwrap();
        assert_eq!(rows_tx(&conn, &id).unwrap()[0].delivered_at, Some(11));
    }

    /// A delivery flush that raced a local read must not clear the newer read, and the read time
    /// is the first one.
    #[test]
    fn local_read_times_follow_arrival_order_and_preserve_in_flight_updates() {
        let (conn, conv) = db();
        let arrive = |n: u8, did: [u8; 16], author: [u8; 32], now: u64| {
            conn.execute(
                "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, timestamp, \
                 status, dispatch_id) VALUES (?1, ?2, ?3, 'hi', 0, 1, 1, ?4)",
                (ulid(n), conv.as_slice(), author.as_slice(), did.as_slice()),
            )
            .unwrap();
            arrived_tx(&conn, &ulid(n), now).unwrap();
        };
        let read_at = |n: u8| -> (bool, Option<u64>, bool) {
            conn.query_row(
                "SELECT is_read, read_at, pending FROM incoming_receipts WHERE message_id = ?1",
                [ulid(n)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        // Arrival order, not each author's dispatch-id clock, decides what a read covers.
        arrive(1, [9; 16], A, 100);
        arrive(2, [2; 16], B, 101);
        read_tx(&conn, &conv, &[2; 16], 110).unwrap();
        assert_eq!((read_at(1).1, read_at(2).1), (Some(110), Some(110)));
        arrive(3, [1; 16], A, 120);
        assert!(!read_at(3).0, "a later arrival is unread");
        read_tx(&conn, &conv, &[1; 16], 130).unwrap();

        let mut flushed = Pending {
            id: ulid(3),
            conv,
            author: A,
            entry: ReceiptEntry {
                message_id:   [1; 16],
                delivered_at: Some(120),
                read_at:      None,
            },
        };
        finish_batch_tx(&conn, &[flushed.clone()]).unwrap();
        assert!(read_at(3).2, "the read that landed during the flush still goes out");
        flushed.entry.read_at = Some(130);
        finish_batch_tx(&conn, &[flushed]).unwrap();
        assert!(!read_at(3).2);
        read_tx(&conn, &conv, &[1; 16], 150).unwrap();
        assert_eq!(read_at(3).1, Some(130));
    }
}
