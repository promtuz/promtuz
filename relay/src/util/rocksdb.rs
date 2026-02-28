use anyhow::Result;
use rust_rocksdb::DB;
use rust_rocksdb::Options;
use rust_rocksdb::SliceTransform;

// static ROCKS_DB_FILE: &str = "relay.db";

pub fn rocksdb() -> Result<DB> {
    let mut opts = Options::default();
    
    opts.create_if_missing(true);

    // to scan all messages queues of a recipient
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));

    let db = DB::open(&opts, "db")?;

    Ok(db)
}
