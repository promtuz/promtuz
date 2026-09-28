//! Recipient-scoped delivery history. A relay acknowledgement is sent, never
//! delivered/read. Original audiences survive roster changes and partial failure.
use super::message::{STATUS_DELIVERED, STATUS_FAILED, STATUS_PENDING, STATUS_READ, STATUS_SENT};
use crate::db::messages::MESSAGES_DB;
use anyhow::{Result, ensure};
use common::proto::mls_wire::{AppPayload, ReceiptDetails, ReceiptEntry, ReceiptKind};
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
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    // An older message has no provable full audience. Preserve that limitation.
    snapshot_tx(&tx, id, fallback, false)?;
    let peers = tx
        .prepare("SELECT member FROM message_recipients WHERE message_id=?1")?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(peers)
}

fn rows_tx(conn: &Connection, id: &str) -> Result<Vec<RecipientReceipt>> {
    Ok(conn
        .prepare(ROWS)?
        .query_map([id], |r| {
            Ok(RecipientReceipt {
                member: r.get(0)?,
                name: String::new(),
                sent_at: r.get(1)?,
                delivered_at: r.get(2)?,
                read_at: r.get(3)?,
                status: r.get(4)?,
                active: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
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
    let at = at.filter(|time| *time > 0 && *time <= i64::MAX as u64);
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    send_result_tx(&tx, dispatch, &member, status, at)?;
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
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE message_recipients SET send_status=2 WHERE message_id=?1 AND send_status=0",
        [id],
    )?;
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_recipients WHERE message_id=?1)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        aggregate_tx(&tx, id)?;
    } else {
        tx.execute("UPDATE messages SET status=2 WHERE id=?1 AND status<3", [id])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn info(conv: &[u8; 16], dispatch: &[u8; 16]) -> Result<MessageReceiptInfo> {
    let (complete, mut recipients) = {
        let conn = MESSAGES_DB.lock();
        let id:String=conn.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=1 AND deleted=0",(conv.as_slice(),dispatch.as_slice()),|r|r.get(0))?;
        let complete = conn
            .query_row("SELECT complete FROM message_audiences WHERE message_id=?1", [&id], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or(false);
        (complete, rows_tx(&conn, &id)?)
    };
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

pub(crate) fn receive(conv: &[u8; 16], member: &[u8; 32], payload: AppPayload) -> Result<()> {
    let mut conn = MESSAGES_DB.lock();
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
            conn.execute(
                "INSERT OR IGNORE INTO receipt_peers VALUES (?1,?2)",
                (conv.as_slice(), member.as_slice()),
            )?;
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
            }
            // Activating exact receipts also retires provisional legacy inferences
            // for other messages to this upgraded member, including skipped IDs.
            changed=conn.prepare("SELECT r.message_id FROM message_recipients r JOIN messages m ON m.id=r.message_id WHERE m.conversation_id=?1 AND r.member=?2")?
                .query_map((conv.as_slice(),member.as_slice()),|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        },
        AppPayload::Receipt { kind, upto } => {
            // Old clients broadcast watermarks for every author's IDs. Require
            // the named target to be OUR post sent to this reporting member.
            let known:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN message_recipients r ON r.message_id=m.id
                WHERE m.conversation_id=?1 AND m.dispatch_id=?2 AND m.outgoing=1 AND r.member=?3)",
                (conv.as_slice(),upto.as_slice(),member.as_slice()),|r|r.get(0))?;
            if !known {
                return Ok(());
            }
            changed=conn.prepare("SELECT r.message_id FROM message_recipients r JOIN messages m ON m.id=r.message_id
                WHERE m.conversation_id=?1 AND m.dispatch_id<=?2 AND m.outgoing=1 AND r.member=?3")?
                .query_map((conv.as_slice(),upto.as_slice(),member.as_slice()),|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let status = match kind {
                ReceiptKind::Delivered => STATUS_DELIVERED,
                ReceiptKind::Read => STATUS_READ,
            };
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

pub(super) fn arrived_tx(conn: &Connection, id: &str, now: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO incoming_receipts(message_id,delivered_at,pending) VALUES (?1,?2,1)",
        (id, now),
    )?;
    Ok(())
}

pub(crate) fn read(conv: &[u8; 16], upto: &[u8; 16]) -> Result<()> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    read_tx(&tx, conv, upto, crate::utils::systime().as_secs())?;
    tx.commit()?;
    Ok(())
}

pub(super) fn read_tx(conn: &Connection, conv: &[u8; 16], upto: &[u8; 16], now: u64) -> Result<()> {
    // The UI anchor is arrival order, not a comparison between different
    // authors' clocks. Only messages actually present on this device are read.
    conn.execute("INSERT OR IGNORE INTO incoming_receipts(message_id,is_read)
        SELECT m.id,CASE WHEN r.upto_dispatch_id IS NOT NULL AND m.dispatch_id<=r.upto_dispatch_id THEN 1 ELSE 0 END
        FROM messages m LEFT JOIN read_state r ON r.conversation_id=m.conversation_id
        WHERE m.conversation_id=?1 AND m.outgoing=0 AND m.system=0 AND m.dispatch_id IS NOT NULL",[conv.as_slice()])?;
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
    let conn = MESSAGES_DB.lock();
    let groups=conn.prepare("SELECT m.conversation_id,m.sender_ipk FROM incoming_receipts r JOIN messages m ON m.id=r.message_id
        JOIN conversations c ON c.id=m.conversation_id WHERE r.pending=1 AND m.deleted=0 AND c.mls_group_id IS NOT NULL
        AND EXISTS(SELECT 1 FROM conversation_members cm WHERE cm.conversation_id=m.conversation_id AND cm.member_ipk=m.sender_ipk AND cm.active=1)
        GROUP BY m.conversation_id,m.sender_ipk ORDER BY MIN(m.id)")?
        .query_map([],|r|Ok((r.get::<_,[u8;16]>(0)?,r.get::<_,[u8;32]>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let first = groups.into_iter().find(|group| !skip.contains(group));
    let Some((conv, author)) = first else { return Ok(Vec::new()) };
    Ok(conn.prepare("SELECT m.id,m.dispatch_id,r.delivered_at,r.read_at FROM incoming_receipts r JOIN messages m ON m.id=r.message_id
        WHERE r.pending=1 AND m.deleted=0 AND m.conversation_id=?1 AND m.sender_ipk=?2 ORDER BY m.id LIMIT 128")?
        .query_map((conv.as_slice(),author.as_slice()),|r|Ok(Pending {id:r.get(0)?,conv,author,
            entry:ReceiptEntry {message_id:r.get(1)?,delivered_at:r.get(2)?,read_at:r.get(3)?}}))?.collect::<rusqlite::Result<_>>()?)
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
    crate::RUNTIME.spawn(async {
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
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        finish_batch_tx(&tx, &rows)?;
        tx.commit()?;
    }
}

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
    // Compatibility is targeted to that author, never the whole group.
    // New peers prefer exact entries over these time-less watermarks.
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

pub(crate) fn seen_count(conv: &[u8; 16], did: &[u8; 16]) -> u32 {
    let conn = MESSAGES_DB.lock();
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=1",
            (conv.as_slice(), did.as_slice()),
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    id.and_then(|id| rows_tx(&conn, &id).ok())
        .map_or(0, |rows| rows.iter().filter(|r| r.status == STATUS_READ).count() as u32)
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

pub(crate) fn dump() -> Result<Backup> {
    dump_tx(&MESSAGES_DB.lock())
}
fn dump_tx(conn: &Connection) -> Result<Backup> {
    Ok(Backup {
        audiences:conn.prepare("SELECT message_id,complete FROM message_audiences")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?,
        recipients:conn.prepare("SELECT message_id,member,send_status,sent_at,delivered_at,read_at,legacy_status FROM message_recipients")?
            .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?.collect::<rusqlite::Result<_>>()?,
        incoming:conn.prepare("SELECT message_id,delivered_at,read_at,is_read,pending FROM incoming_receipts")?
            .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<rusqlite::Result<_>>()?,
        peers:conn.prepare("SELECT conversation_id,member FROM receipt_peers")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?,
    })
}

pub(crate) fn restore(backup: &Backup) -> Result<()> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    restore_tx(&tx, backup)?;
    tx.commit()?;
    Ok(())
}
fn restore_tx(conn: &Connection, backup: &Backup) -> Result<()> {
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
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    let ids = tx
        .prepare("SELECT id FROM messages WHERE conversation_id=?1 AND outgoing=1 AND status<3")?
        .query_map([conv.as_slice()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        tx.execute("UPDATE message_recipients SET send_status=2 WHERE message_id=?1", [&id])?;
        tx.execute("UPDATE messages SET status=2 WHERE id=?1", [&id])?;
        aggregate_tx(&tx, &id)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CONV: [u8; 16] = [9; 16];
    const A: [u8; 32] = [2; 32];
    const B: [u8; 32] = [3; 32];
    fn db() -> Connection {
        let conn = crate::db::messages::open_in_memory();
        conn.execute(
            "INSERT INTO conversations(id,kind,created_at) VALUES (?1,1,1)",
            [CONV.as_slice()],
        )
        .unwrap();
        for peer in [A, B] {
            conn.execute(
                "INSERT INTO conversation_members(conversation_id,member_ipk) VALUES (?1,?2)",
                (CONV.as_slice(), peer.as_slice()),
            )
            .unwrap();
        }
        conn
    }
    fn post(conn: &Connection, n: u8) -> (String, [u8; 16]) {
        let id = format!("{n:026}");
        let did = [n; 16];
        conn.execute("INSERT INTO messages(id,conversation_id,content,outgoing,timestamp,status,dispatch_id) VALUES (?1,?2,'hello',1,1,0,?3)",
            (&id,CONV.as_slice(),did.as_slice())).unwrap();
        snapshot_tx(conn, &id, &[A, B], true).unwrap();
        (id, did)
    }
    fn state(conn: &Connection, id: &str) -> u8 {
        conn.query_row("SELECT status FROM messages WHERE id=?1", [id], |r| r.get(0)).unwrap()
    }
    fn exact(conn: &Connection, peer: &[u8; 32], did: [u8; 16], d: Option<u64>, r: Option<u64>) {
        receive_tx(
            conn,
            &CONV,
            peer,
            AppPayload::ReceiptDetails(ReceiptDetails {
                entries: vec![ReceiptEntry { message_id: did, delivered_at: d, read_at: r }],
            }),
        )
        .unwrap();
    }
    #[test]
    fn mixed_send_results_are_order_independent_and_do_not_downgrade_receipts() {
        for reversed in [false, true] {
            let conn = db();
            let (id, did) = post(&conn, 1);
            let mut results = [(A, STATUS_SENT, Some(10)), (B, STATUS_FAILED, None)];
            if reversed {
                results.reverse()
            }
            for (peer, status, at) in results {
                send_result_tx(&conn, &did, &peer, status, at).unwrap();
            }
            assert_eq!(state(&conn, &id), STATUS_SENT);
            assert_eq!(
                rows_tx(&conn, &id).unwrap().iter().filter(|r| r.status == STATUS_FAILED).count(),
                1
            );
            exact(&conn, &A, did, Some(11), Some(12));
            exact(&conn, &B, did, Some(13), None);
            assert_eq!(
                state(&conn, &id),
                STATUS_DELIVERED,
                "one delivered + one read is not all read"
            );
            exact(&conn, &B, did, None, Some(14));
            send_result_tx(&conn, &did, &A, STATUS_FAILED, None).unwrap();
            send_result_tx(&conn, &did, &B, STATUS_SENT, Some(20)).unwrap();
            assert_eq!(state(&conn, &id), STATUS_READ);
            exact(&conn, &B, did, Some(19), Some(21));
            let rows = rows_tx(&conn, &id).unwrap();
            let b = rows.iter().find(|r| r.member == B).unwrap();
            assert_eq!(b.delivered_at, Some(13));
            assert_eq!(b.read_at, Some(14));
        }
        let conn = db();
        let (id, did) = post(&conn, 1);
        send_result_tx(&conn, &did, &A, STATUS_FAILED, None).unwrap();
        assert_eq!(state(&conn, &id), STATUS_PENDING);
        send_result_tx(&conn, &did, &B, STATUS_FAILED, None).unwrap();
        assert_eq!(state(&conn, &id), STATUS_FAILED);
    }
    #[test]
    fn exact_ids_ignore_legacy_holes_and_watermarks_from_another_author() {
        let conn = db();
        let (first, a) = post(&conn, 1);
        let (second, b) = post(&conn, 2);
        for did in [a, b] {
            for peer in [A, B] {
                send_result_tx(&conn, &did, &peer, STATUS_SENT, Some(10)).unwrap();
            }
        }
        // Reordered old compatibility receipt can precede exact support. Once
        // exact evidence arrives it must not retain an inferred skipped post.
        receive_tx(&conn, &CONV, &A, AppPayload::Receipt { kind: ReceiptKind::Read, upto: b })
            .unwrap();
        exact(&conn, &A, b, Some(11), Some(12));
        assert_eq!(rows_tx(&conn, &first).unwrap()[0].status, STATUS_SENT);
        assert_eq!(rows_tx(&conn, &second).unwrap()[0].status, STATUS_READ);
        receive_tx(&conn, &CONV, &A, AppPayload::Receipt { kind: ReceiptKind::Read, upto: b })
            .unwrap();
        assert_eq!(rows_tx(&conn, &first).unwrap()[0].status, STATUS_SENT);
        receive_tx(
            &conn,
            &CONV,
            &B,
            AppPayload::Receipt { kind: ReceiptKind::Read, upto: [9; 16] },
        )
        .unwrap();
        assert_eq!(
            rows_tx(&conn, &second).unwrap()[1].status,
            STATUS_SENT,
            "foreign author's watermark cannot mark our messages"
        );
        receive_tx(&conn, &CONV, &B, AppPayload::Receipt { kind: ReceiptKind::Delivered, upto: b })
            .unwrap();
        assert_eq!(state(&conn, &second), STATUS_DELIVERED);
        let rows = rows_tx(&conn, &second).unwrap();
        assert_eq!(rows[1].delivered_at, None, "old peers have no event time");
    }
    #[test]
    fn original_audience_survives_joins_leaves_and_foreign_receipts() {
        let conn = db();
        let (id, did) = post(&conn, 1);
        let c = [4; 32];
        conn.execute(
            "INSERT INTO conversation_members(conversation_id,member_ipk) VALUES (?1,?2)",
            (CONV.as_slice(), c.as_slice()),
        )
        .unwrap();
        snapshot_tx(&conn, &id, &[A, B, c], true).unwrap();
        exact(&conn, &c, did, Some(11), Some(12));
        assert_eq!(rows_tx(&conn, &id).unwrap().len(), 2);
        conn.execute(
            "UPDATE conversation_members SET active=0 WHERE member_ipk=?1",
            [B.as_slice()],
        )
        .unwrap();
        exact(&conn, &A, did, Some(11), Some(12));
        assert_eq!(state(&conn, &id), STATUS_SENT);
        assert!(!rows_tx(&conn, &id).unwrap()[1].active);
        exact(&conn, &B, did, Some(13), Some(14));
        assert_eq!(
            state(&conn, &id),
            STATUS_READ,
            "a delayed receipt from an original recipient still counts"
        );
        let before = rows_tx(&conn, &id).unwrap()[0].read_at;
        receive_tx(
            &conn,
            &[8; 16],
            &A,
            AppPayload::ReceiptDetails(ReceiptDetails {
                entries: vec![ReceiptEntry {
                    message_id: did,
                    delivered_at: None,
                    read_at: Some(1),
                }],
            }),
        )
        .unwrap();
        assert_eq!(rows_tx(&conn, &id).unwrap()[0].read_at, before);
    }
    #[test]
    fn receipt_transaction_failure_cannot_publish_half_an_update() {
        let mut conn = db();
        let (id, did) = post(&conn, 1);
        conn.execute_batch("CREATE TRIGGER reject_receipt BEFORE UPDATE ON message_recipients BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(
                receive_tx(
                    &tx,
                    &CONV,
                    &A,
                    AppPayload::ReceiptDetails(ReceiptDetails {
                        entries: vec![ReceiptEntry {
                            message_id: did,
                            delivered_at: Some(11),
                            read_at: None
                        }]
                    })
                )
                .is_err()
            );
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM receipt_peers", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(state(&conn, &id), STATUS_PENDING);
        conn.execute_batch("DROP TRIGGER reject_receipt;").unwrap();
        {
            let tx = conn.transaction().unwrap();
            exact(&tx, &A, did, Some(11), None);
            tx.commit().unwrap();
        }
        let path =
            std::env::temp_dir().join(format!("promtuz-receipt-{}.sqlite", uuid::Uuid::now_v7()));
        conn.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(rows_tx(&conn, &id).unwrap()[0].delivered_at, Some(11));
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn backup_preserves_times_without_broadening_live_audiences() {
        let source = db();
        let (id, did) = post(&source, 1);
        exact(&source, &A, did, Some(10), Some(12));
        send_result_tx(&source, &did, &B, STATUS_FAILED, None).unwrap();
        let bytes = postcard::to_allocvec(&dump_tx(&source).unwrap()).unwrap();
        let backup: Backup = postcard::from_bytes(&bytes).unwrap();
        let restored = db();
        post(&restored, 1);
        restored.execute("DELETE FROM message_recipients", []).unwrap();
        restored.execute("DELETE FROM message_audiences", []).unwrap();
        restore_tx(&restored, &backup).unwrap();
        restore_tx(&restored, &backup).unwrap();
        assert_eq!(rows_tx(&restored, &id).unwrap()[0].read_at, Some(12));
        assert_eq!(rows_tx(&restored, &id).unwrap()[1].status, STATUS_FAILED);
        restored.execute("DELETE FROM message_recipients WHERE member=?1", [B.as_slice()]).unwrap();
        restore_tx(&restored, &backup).unwrap();
        assert_eq!(
            rows_tx(&restored, &id).unwrap().len(),
            1,
            "existing audience remains authoritative"
        );
        restored.execute("DELETE FROM messages", []).unwrap();
        assert_eq!(
            restored
                .query_row("SELECT COUNT(*) FROM message_recipients", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn local_read_times_follow_arrival_order_and_preserve_in_flight_updates() {
        let conn = db();
        let insert = |id: &str, did: [u8; 16], author: [u8; 32], now: u64| {
            conn.execute("INSERT INTO messages(id,conversation_id,sender_ipk,content,outgoing,timestamp,status,dispatch_id) VALUES (?1,?2,?3,'hi',0,1,1,?4)",
                (id,CONV.as_slice(),author.as_slice(),did.as_slice())).unwrap();
            arrived_tx(&conn, id, now).unwrap();
        };
        insert("00000000000000000000000001", [9; 16], A, 100);
        insert("00000000000000000000000002", [2; 16], B, 101);
        read_tx(&conn, &CONV, &[2; 16], 110).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM incoming_receipts WHERE read_at=110", [], |r| r
                .get::<_, u32>(
                0
            ))
            .unwrap(),
            2
        );
        insert("00000000000000000000000003", [1; 16], A, 120);
        assert_eq!(conn.query_row("SELECT is_read FROM incoming_receipts WHERE message_id='00000000000000000000000003'",[],|r|r.get::<_,bool>(0)).unwrap(),false);
        read_tx(&conn, &CONV, &[1; 16], 130).unwrap();
        // A delivery flush that raced the read must not clear its newer work.
        let mut flushed = Pending {
            id: "00000000000000000000000003".into(),
            conv: CONV,
            author: A,
            entry: ReceiptEntry { message_id: [1; 16], delivered_at: Some(120), read_at: None },
        };
        finish_batch_tx(&conn, &[flushed.clone()]).unwrap();
        assert!(
            conn.query_row(
                "SELECT pending FROM incoming_receipts WHERE delivered_at=120",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        flushed.entry.read_at = Some(130);
        finish_batch_tx(&conn, &[flushed]).unwrap();
        assert!(
            !conn
                .query_row(
                    "SELECT pending FROM incoming_receipts WHERE delivered_at=120",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        read_tx(&conn, &CONV, &[1; 16], 150).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT read_at FROM incoming_receipts WHERE delivered_at=120",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            130
        );
    }
}
