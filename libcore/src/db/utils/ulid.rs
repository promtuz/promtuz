use rusqlite::ToSql;
use rusqlite::types::FromSql;
use rusqlite::types::FromSqlError;
use rusqlite::types::FromSqlResult;
use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef;
use serde::Deserialize;
use serde::Serialize;
use ulid::Ulid;

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub struct ULID(Ulid);

impl From<Ulid> for ULID {
    fn from(value: Ulid) -> Self {
        Self(value)
    }
}

impl From<ULID> for ulid::Ulid {
    fn from(val: ULID) -> Self {
        val.0
    }
}

impl std::ops::Deref for ULID {
    type Target = ulid::Ulid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromSql for ULID {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<ulid::Ulid>()
            .map(ULID)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for ULID {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.to_string()))
    }
}
