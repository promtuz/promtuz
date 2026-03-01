use common::proto::client_rel::DeliverP;
use common::proto::pack::Unpacker;
use common::quic::id::UserId;
use rust_rocksdb::DB;
use rust_rocksdb::Options;
use rust_rocksdb::SliceTransform;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Debug, KnownLayout, FromBytes, IntoBytes, Immutable)]
#[repr(C, packed)]
pub struct MessageKey {
    pub recipient: [u8; 32],
    pub timestamp: [u8; 8],
    pub rand: [u8; 4],
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));

    let db = DB::open(&opts, "./db")?;

    let mut iter = db.iterator(rust_rocksdb::IteratorMode::Start);

    while let Some(Ok((key, value))) = iter.next() {
        let key = MessageKey::read_from_bytes(&key[..]).unwrap();
        let time = u64::from_be_bytes(key.timestamp);

        let msg = DeliverP::deser(&value[..]).map_err(|_| value);

        println!(
            "{ipk} | {time} | {rand} - {msg:?}",
            ipk = UserId::derive(&key.recipient),
            rand = hex::encode(key.rand)
        );
    }

    Ok(())
}