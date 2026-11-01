//! Private-history suffix: store the original ciphertext and code-wrapped key, never decrypted
//! rows, even when export happens during an unlocked session. The identity backup key is only an
//! outer transport envelope and cannot unlock these histories.
use anyhow::{Result, ensure};
use rusqlite::{
    Connection,
    types::{Value, ValueRef},
};
use serde::{Deserialize, Serialize};
const TABLES: &[&str] = &[
    "conversations",
    "conversation_members",
    "private_conversations",
    "messages",
    "private_reactions",
    "message_media",
    "read_state",
    "member_read_state",
    "group_pictures",
    "private_files",
    "message_media_preparations",
    "private_file_refs",
];
#[derive(Serialize, Deserialize)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}
impl Cell {
    fn read(value: ValueRef<'_>) -> Result<Self> {
        Ok(match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(v) => Self::Integer(v),
            ValueRef::Real(v) => Self::Real(v),
            ValueRef::Text(v) => Self::Text(std::str::from_utf8(v)?.into()),
            ValueRef::Blob(v) => Self::Blob(v.into()),
        })
    }
    fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(v) => Value::Integer(*v),
            Self::Real(v) => Value::Real(*v),
            Self::Text(v) => Value::Text(v.clone()),
            Self::Blob(v) => Value::Blob(v.clone()),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Table {
    name: String,
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct History {
    id: [u8; 16],
    public: Vec<u8>,
    tables: Vec<Table>,
}
fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    Ok(crate::db::all(conn, &format!("PRAGMA table_info({table})"), [], |r| r.get(1))?)
}
pub(crate) fn dump(conn: &Connection) -> Result<Vec<History>> {
    let ids = crate::privacy::ids_tx(conn)?;
    let mut histories = Vec::new();
    for id in ids {
        let public = conn.query_row(
            "SELECT public_key FROM private_conversations WHERE conversation_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let mut tables = Vec::new();
        for &name in TABLES {
            let columns = columns(conn, name)?;
            // Attachment bytes are not in an account backup. Paths, local file keys and a
            // completed flag describe this device's copy, not a destination capability.
            if name == "private_files" {
                tables.push(Table { name: name.into(), columns, rows: Vec::new() });
                continue;
            }
            let key = if name == "conversations" { "id" } else { "conversation_id" };
            let mut query = conn.prepare(&format!("SELECT * FROM {name} WHERE {key}=?1"))?;
            let mut rows = query.query([id])?;
            let mut saved = Vec::new();
            while let Some(row) = rows.next()? {
                let mut cells = Vec::new();
                for i in 0..columns.len() {
                    cells.push(Cell::read(row.get_ref(i)?)?)
                }
                saved.push(cells);
            }
            tables.push(Table { name: name.into(), columns, rows: saved });
        }
        histories.push(History { id, public, tables });
    }
    Ok(histories)
}
pub(crate) fn restore(conn: &Connection, histories: &[History]) -> Result<()> {
    use rusqlite::OptionalExtension;
    for history in histories {
        let live: Option<Vec<u8>> = conn
            .query_row(
                "SELECT public_key FROM private_conversations WHERE conversation_id=?1",
                [history.id],
                |r| r.get(0),
            )
            .optional()?;
        if live.as_ref().is_some_and(|public| public != &history.public) {
            continue;
        }
        if live.is_none()
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
                [history.id],
                |r| r.get::<_, bool>(0),
            )?
        {
            continue;
        }
        ensure!((8..=TABLES.len()).contains(&history.tables.len()), "invalid private backup tables");
        for (table, &expected) in history.tables.iter().zip(TABLES) {
            ensure!(table.name == expected, "invalid private backup table order");
            let actual = columns(conn, expected)?;
            ensure!(table.columns == actual, "unsupported private backup schema");
            // Older suffixes carried these rows without their files. Retain any valid local
            // copy already owned by this device and let restored media fetch its own bytes.
            if expected == "private_files" { continue; }
            let names =
                table.columns.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
            let params = (1..=actual.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(",");
            for row in &table.rows {
                ensure!(row.len() == actual.len(), "invalid private backup row");
                let values = row.iter().map(Cell::value).collect::<Vec<_>>();
                conn.execute(
                    &format!("INSERT OR IGNORE INTO {expected}({names}) VALUES({params})"),
                    rusqlite::params_from_iter(values),
                )?;
            }
        }
        // History remains code protected; ordinary session recovery creates a new MLS session.
        if live.is_none() {
            conn.execute("UPDATE conversations SET mls_group_id=NULL WHERE id=?1 AND kind=0", [history.id])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn restore_does_not_import_another_devices_private_file_capabilities() {
        let source = crate::test_support::ScopedCore::new();
        let id = [58u8;16];
        source.core.db.messages().lock().execute("INSERT INTO conversations(id) VALUES(?1)",[id]).unwrap();
        crate::privacy::hide(&id,"file-backup-code").unwrap();
        let mut history = dump(&source.core.db.messages().lock()).unwrap();
        // Older backups can contain a path and complete flag without the actual encrypted
        // attachment. The destination must obtain bytes through its authenticated media offer.
        let files = history[0].tables.iter_mut().find(|t|t.name=="private_files").unwrap();
        files.rows.push(vec![Cell::Blob(id.to_vec()),Cell::Blob(vec![59;32]),
            Cell::Text("/private/tmp/source-device-only".into()),Cell::Blob(vec![60;32]),Cell::Integer(1)]);
        let destination = crate::test_support::ScopedCore::new();
        restore(&destination.core.db.messages().lock(), &history).unwrap();
        assert_eq!(destination.core.db.messages().lock().query_row(
            "SELECT count(*) FROM private_files",[],|r|r.get::<_,u32>(0),
        ).unwrap(),0);
        destination.core.db.messages().lock().execute(
            "INSERT INTO private_files(conversation_id,file_id,path,details,complete) VALUES(?1,?2,'/private/tmp/destination-owned',?3,1)",
            (id.as_slice(),[59u8;32].as_slice(),[61u8;32].as_slice()),
        ).unwrap();
        restore(&destination.core.db.messages().lock(), &history).unwrap();
        let path:String=destination.core.db.messages().lock().query_row(
            "SELECT path FROM private_files",[],|r|r.get(0),
        ).unwrap();
        assert_eq!(path,"/private/tmp/destination-owned");
    }

    #[tokio::test]
    async fn private_backup_preserves_ciphertext_and_only_the_code_unlocks_restored_history() {
        let source = crate::test_support::ScopedCore::new();
        let id = [55u8; 16];
        source
            .core
            .db
            .messages()
            .lock()
            .execute("INSERT INTO conversations(id) VALUES(?1)", [id])
            .unwrap();
        crate::data::message::Message::save_incoming(
            id,
            [56; 32],
            &[57; 16],
            "private backup secret",
            1,
            None,
        )
        .unwrap();
        crate::privacy::hide(&id, "backup secret code").unwrap();
        crate::privacy::unlock("backup secret code").unwrap();
        let history = dump(&source.core.db.messages().lock()).unwrap();
        let bytes = postcard::to_allocvec(&history).unwrap();
        assert!(
            !bytes.windows(b"private backup secret".len()).any(|w| w == b"private backup secret")
        );
        let destination = crate::test_support::ScopedCore::new();
        restore(&destination.core.db.messages().lock(), &history).unwrap();
        assert!(crate::api::messaging::get_messages(id.to_vec(), 100, "".into()).is_err());
        assert!(crate::privacy::unlock("wrong code").unwrap().is_none());
        assert_eq!(crate::privacy::unlock("backup secret code").unwrap(), Some(id));
        assert_eq!(
            crate::data::message::Message::get_messages(&id, 100, "")[0].content,
            "private backup secret"
        );
    }
}
