use common::proto::client_rel::DeliverP;
use common::proto::pack::Unpacker;
use common::quic::id::UserId;
use relay::storage::MessageKey;
use relay::storage::db::Store;

fn main() -> anyhow::Result<()> {
    let store = Store::open("./db")?;

    for guard in store.messages.iter() {
        let (key, value) = guard.into_inner()?;
        let Some(parsed) = MessageKey::parse(&key[..]) else {
            eprintln!("invalid key length: {}", key.len());
            continue;
        };
        let time = u64::from_be_bytes(parsed.ts_be);
        let msg = DeliverP::deser(&value[..]).map_err(|_| value.to_vec());

        println!(
            "{ipk} | {time} | {id} - {msg:?}",
            ipk = UserId::derive(&parsed.recipient),
            id = hex::encode(parsed.id)
        );
    }

    Ok(())
}
