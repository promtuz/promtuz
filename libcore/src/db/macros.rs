macro_rules! from_row {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl $ty {
            pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
                Ok(Self {
                    $(
                        $field: row.get(stringify!($field))?,
                    )*
                })
            }
        }
    };
}

pub(super) use from_row;

macro_rules! PRAGMA {
    ($conn:expr, $MIGRATIONS:expr) => {
        // Set PRAGMAs before migrations
        $conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        $conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        if cfg!(target_os = "android") {
            $conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
            $conn.pragma_update(None, "temp_store", "MEMORY").unwrap();
        }

        // Migrations
        $MIGRATIONS.to_latest(&mut $conn).expect("db migration failed");
    };
}

pub(super) use PRAGMA;