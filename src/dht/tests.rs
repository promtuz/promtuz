#[cfg(test)]
// mod tests {
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;

use crate::dht::routing::xor_distance;
use crate::dht::*;

fn sample_record() -> UserRecord {
    let sk = SigningKey::generate(&mut OsRng);
    let ipk = sk.verifying_key().to_bytes();
    let relay = NodeId::from_bytes([0xAA; NodeId::LEN]);
    let relay_addr = "127.0.0.1:5001".parse().unwrap();
    let mut rec = UserRecord {
        ipk,
        relay,
        relay_addr,
        timestamp: 1,
        signature: None,
        metadata: UserMetadata { status: Some("online".into()) },
    };
    let sig = sk.sign(&rec.signing_payload());
    rec.signature = Some(sig.to_bytes());
    rec
}

#[test]
fn dht_inserts_and_reads_user() {
    let local_id = NodeId::from_bytes([0x01; NodeId::LEN]);
    let mut dht = Dht::new(local_id, None);

    let rec = sample_record();
    assert!(dht.upsert_user(rec.clone()));
    let fetched = dht.get_user(&rec.ipk).unwrap();
    assert_eq!(fetched.relay, rec.relay);
}

#[test]
fn xor_distance_orders_correctly() {
    let target = NodeId::from_bytes([0x00; NodeId::LEN]);
    let a = NodeId::from_bytes([0x01; NodeId::LEN]);
    let b = NodeId::from_bytes([0x02; NodeId::LEN]);

    let da = xor_distance(target, a);
    let db = xor_distance(target, b);
    assert!(da < db);
}
// }
