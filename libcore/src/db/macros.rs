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

pub(crate) use from_row;
