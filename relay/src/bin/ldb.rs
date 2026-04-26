use common::proto::client_rel::DeliverP;
use common::proto::pack::Unpacker;
use common::quic::id::UserId;
use rust_rocksdb::DB;
use rust_rocksdb::Options;
use rust_rocksdb::SliceTransform;

#[path = "../storage/mod.rs"]
mod storage;

use storage::MessageKey;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));

    let db = DB::open(&opts, "./db")?;

    let mut iter = db.iterator(rust_rocksdb::IteratorMode::Start);

    while let Some(Ok((key, value))) = iter.next() {
        let Some(key) = MessageKey::parse(&key[..]) else {
            eprintln!("invalid key length: {}", key.len());
            continue;
        };
        let time = u64::from_be_bytes(key.ts_be);

        let msg = DeliverP::deser(&value[..]).map_err(|_| value);

        println!(
            "{ipk} | {time} | {id} - {msg:?}",
            ipk = UserId::derive(&key.recipient),
            id = hex::encode(key.id)
        );
    }

    Ok(())
}
