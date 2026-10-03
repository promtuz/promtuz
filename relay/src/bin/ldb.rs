//! Dumps the relay's fjall keyspaces: `cargo run -p relay --bin ldb -- [db_path]` (default `db`).
//! fjall is single-writer, so stop the relay first or the open fails on the directory lock.

use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchP;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::pack::Unpacker;
use relay::storage::MessageKey;
use relay::storage::db::Store;

fn short(ipk: &[u8]) -> String {
    hex::encode(&ipk[..ipk.len().min(6)])
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "db".to_string());
    eprintln!("== opening fjall store at {path} ==");
    let store = Store::open(&path)?;

    println!("\n=== messages ===");
    let mut n = 0;
    for guard in store.messages.iter() {
        let (key, value) = guard.into_inner()?;
        let Some(parsed) = MessageKey::parse(&key[..]) else {
            eprintln!("  invalid messages key len {}", key.len());
            continue;
        };
        let time = u64::from_be_bytes(parsed.ts_be);
        let msg = DeliverP::deser(&value[..]).map(|d| short(&d.from.0)).unwrap_or_else(|_| "??".into());
        println!("  to={} ts={} id={} from={}", short(&parsed.recipient), time, hex::encode(parsed.id), msg);
        n += 1;
    }
    println!("  ({n} rows)");

    println!("\n=== dht_queue (offline messages awaiting drain) ===");
    n = 0;
    for guard in store.queue.iter() {
        let (key, value) = guard.into_inner()?;
        let Some(parsed) = MessageKey::parse(&key[..]) else {
            eprintln!("  invalid queue key len {}", key.len());
            continue;
        };
        let from = DispatchP::deser(&value[..]).map(|d| short(&d.from.0)).unwrap_or_else(|_| "??".into());
        println!(
            "  to={} ts={} dispatch_id={} from={} ({} bytes)",
            short(&parsed.recipient), u64::from_be_bytes(parsed.ts_be), hex::encode(parsed.id), from, value.len()
        );
        n += 1;
    }
    println!("  ({n} rows)");

    println!("\n=== dht_welcome (stashed welcomes) ===");
    n = 0;
    for guard in store.welcome.iter() {
        let (_key, value) = guard.into_inner()?;
        if value.len() < 8 {
            eprintln!("  short welcome value len {}", value.len());
            continue;
        }
        let expires = u64::from_be_bytes(value[..8].try_into().unwrap());
        match WelcomeEnvelopeP::deser(&value[8..]) {
            Ok(env) => println!(
                "  recipient={} sender={} group={} HAS_INVITE={} expires_ms={}",
                short(&env.recipient_ipk.0),
                short(&env.sender_ipk.0),
                short(&env.group_id.0),
                env.pairing.is_some(),
                expires,
            ),
            Err(e) => println!("  <undecodable WelcomeEnvelopeP: {e}> ({} bytes)", value.len()),
        }
        n += 1;
    }
    println!("  ({n} rows)");

    println!("\n=== dht_keypackage (published KeyPackages) ===");
    println!("  ({} rows total)",store.key_packages.count()?);

    Ok(())
}
