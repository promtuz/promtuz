//! MLS Welcome queue at a home relay: publish, fetch and ack over the `dht_welcome` keyspace.

use common::crypto::verify_ed25519;
use common::proto::mls_wire::MAX_WELCOMES_PER_RECIPIENT;
use common::proto::mls_wire::MAX_WELCOME_ACK_IDS;
use common::proto::mls_wire::MAX_WELCOME_BYTES;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WELCOME_ID_LEN;
use common::proto::mls_wire::WELCOME_LIFETIME_MS;
use common::proto::mls_wire::WelcomeAckReq;
use common::proto::mls_wire::WelcomeAckResp;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomeFetchFound;
use common::proto::mls_wire::WelcomeFetchOutcome;
use common::proto::mls_wire::WelcomeFetchReq;
use common::proto::mls_wire::WelcomePublishOutcome;
use common::proto::mls_wire::WelcomePublishReq;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_envelope_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::pack::MAX_FRAME_BYTES;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;

use super::Reject;
use super::hourly_quota;
use super::stash_gate;
use crate::dht::Dht;
use crate::dht::rate_limit::KeyedLimiter;
use crate::dht::rate_limit::LimiterClock;
use crate::dht::rate_limit::keyed;

const STASH_PREFIX_LEN: usize = 32;

/// On-disk key: `stash_prefix(32) || welcome_id(8)`.
const STORAGE_KEY_LEN: usize = STASH_PREFIX_LEN + WELCOME_ID_LEN;

/// Per relay across welcome publish, fetch and ack, on top of the bulk class. Invites are rare.
const MAX_WELCOME_RPC_PER_HOUR: u32 = 240;

/// One inviter's share of a recipient's [`MAX_WELCOMES_PER_RECIPIENT`] slots. Identities are free
/// to mint, so without this one can fill the queue and block every other invitation.
const MAX_WELCOMES_PER_SENDER: usize = 4;

/// Leaves framing headroom under [`MAX_FRAME_BYTES`]. Entries past it stay stored and come back on
/// the fetch after the recipient's ack.
const WELCOME_FETCH_MAX_BYTES: usize = MAX_FRAME_BYTES - 64 * 1024;

/// `BLAKE3("welcome:" || ipk)`: the domain keeps it apart from the KeyPackage stash and from the
/// user's own key in the shared keyspace.
pub fn stash_prefix(ipk: &[u8; 32]) -> [u8; STASH_PREFIX_LEN] {
    *NodeId::new([b"welcome:".as_slice(), ipk].concat()).as_bytes()
}

fn storage_key(ipk: &[u8; 32], welcome_id: &[u8; WELCOME_ID_LEN]) -> [u8; STORAGE_KEY_LEN] {
    let mut k = [0u8; STORAGE_KEY_LEN];
    k[..STASH_PREFIX_LEN].copy_from_slice(&stash_prefix(ipk));
    k[STASH_PREFIX_LEN..].copy_from_slice(welcome_id);
    k
}

/// `BLAKE3(recipient || group_id || kp_ref_used || BLAKE3(welcome_blob))`, truncated. Every home
/// derives the same id, so one `WelcomeAck` clears all replicas and a republish reuses its row.
fn welcome_id(env: &WelcomeEnvelopeP) -> [u8; WELCOME_ID_LEN] {
    let blob_digest = *NodeId::new(&env.welcome_blob.0).as_bytes();
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&env.recipient_ipk.0);
    buf.extend_from_slice(&env.group_id.0);
    buf.extend_from_slice(&env.kp_ref_used.0);
    buf.extend_from_slice(&blob_digest);

    let mut id = [0u8; WELCOME_ID_LEN];
    id.copy_from_slice(&NodeId::new(&buf).as_bytes()[..WELCOME_ID_LEN]);
    id
}

#[derive(Debug)]
pub(crate) struct WelcomeLimiters {
    pub(crate) limiter: KeyedLimiter<[u8; 32]>,
}

impl WelcomeLimiters {
    pub(crate) fn new(clock: &LimiterClock) -> Self {
        Self { limiter: keyed(hourly_quota(MAX_WELCOME_RPC_PER_HOUR), clock) }
    }

    pub(crate) fn check(&self, requester: &NodeId) -> Result<(), ()> {
        self.limiter.check_key(requester.as_bytes()).map_err(|_| ())
    }

    pub(crate) fn sweep(&self) {
        self.limiter.retain_recent();
        self.limiter.shrink_to_fit();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WelcomeVerifyError {
    Malformed,
    BadSig,
}

fn verify_welcome_envelope(env: &WelcomeEnvelopeP) -> Result<(), WelcomeVerifyError> {
    if env.welcome_blob.0.is_empty() {
        return Err(WelcomeVerifyError::Malformed);
    }
    if env.welcome_blob.0.len() > MAX_WELCOME_BYTES {
        return Err(WelcomeVerifyError::Malformed);
    }

    let msg = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &env.group_id.0,
        &env.sender_ipk.0,
        &env.recipient_ipk.0,
        &env.kp_ref_used.0,
        &env.welcome_blob.0,
    );
    verify_ed25519(&env.sender_ipk.0, &msg, &env.sender_sig.0)
        .map_err(|_| WelcomeVerifyError::BadSig)
}

fn iterate_welcomes(
    dht: &Dht, ipk: &[u8; 32],
) -> Vec<([u8; STORAGE_KEY_LEN], WelcomeEnvelopeP, u64, usize)> {
    let mut out = Vec::new();
    let prefix = stash_prefix(ipk);
    for guard in dht.store.welcome.prefix(prefix) {
        let (key_bytes, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => break,
        };
        let Ok(k) = <[u8; STORAGE_KEY_LEN]>::try_from(&*key_bytes) else {
            continue;
        };
        // Stored value layout: `expires_at_ms (BE u64) || postcard(envelope)`.
        let Some((expires_at_ms, envelope)) = value.split_first_chunk::<8>() else {
            continue;
        };
        let Ok(env) = WelcomeEnvelopeP::deser(envelope) else {
            continue;
        };
        out.push((k, env, u64::from_be_bytes(*expires_at_ms), value.len()));
    }
    out
}

/// A republish of an id already on disk overwrites its own row and is never refused.
fn welcome_queue_full(
    dht: &Dht, ipk: &[u8; 32], sender_ipk: &[u8; 32], key: &[u8; STORAGE_KEY_LEN],
) -> bool {
    let mut total: usize = 0;
    let mut from_sender: usize = 0;
    for (k, env, _, _) in iterate_welcomes(dht, ipk) {
        if k == *key {
            return false;
        }
        total += 1;
        if env.sender_ipk.0 == *sender_ipk {
            from_sender += 1;
        }
    }
    total >= MAX_WELCOMES_PER_RECIPIENT || from_sender >= MAX_WELCOMES_PER_SENDER
}

/// The envelope's `sender_sig` binds it to the inviter, so a publish is accepted from any relay.
/// `Stored` is a durable promise, so it waits for the persist barrier.
pub(crate) async fn handle_welcome_publish(
    dht: &Dht, req: WelcomePublishReq, peer: NodeId, now_ms: u64,
) -> WelcomePublishOutcome {
    let recipient_short = hex::encode(&req.envelope.recipient_ipk.0[..4]);
    let admit = || dht.welcome_limiters.check(&peer).is_ok();
    let stash = stash_prefix(&req.envelope.recipient_ipk.0);
    if let Err(reject) = stash_gate(dht, peer, None, req.timestamp, now_ms, stash, admit) {
        common::debug!("MLS welcome_publish: {reject:?} for recipient={recipient_short}");
        return match reject {
            Reject::Skew => WelcomePublishOutcome::StaleTimestamp,
            Reject::RateLimited => WelcomePublishOutcome::RateLimited,
            Reject::Binding | Reject::NotOwner => WelcomePublishOutcome::NotOwner,
        };
    }

    match verify_welcome_envelope(&req.envelope) {
        Ok(()) => {}
        Err(e) => {
            common::warn!(
                "MLS welcome_publish: envelope sig verify failed for recipient={}: {e:?}",
                recipient_short
            );
            return WelcomePublishOutcome::BadSig;
        }
    }

    let id = welcome_id(&req.envelope);
    let key = storage_key(&req.envelope.recipient_ipk.0, &id);
    if welcome_queue_full(
        dht,
        &req.envelope.recipient_ipk.0,
        &req.envelope.sender_ipk.0,
        &key,
    ) {
        common::warn!(
            "MLS welcome_publish: queue full for recipient={}",
            recipient_short
        );
        return WelcomePublishOutcome::QueueFull;
    }

    let envelope_bytes = match req.envelope.ser() {
        Ok(b) => b,
        Err(e) => {
            common::warn!("MLS welcome_publish: envelope encode failed: {e:?}");
            return WelcomePublishOutcome::BadSig;
        }
    };

    let expires_at_ms = now_ms.saturating_add(WELCOME_LIFETIME_MS);
    let mut value = Vec::with_capacity(8 + envelope_bytes.len());
    value.extend_from_slice(&expires_at_ms.to_be_bytes());
    value.extend_from_slice(&envelope_bytes);

    if let Err(e) = dht.store.put_sync(&dht.store.welcome, key, &value) {
        common::warn!(
            "MLS welcome_publish: fjall put failed for recipient={}: {e}",
            recipient_short
        );
        return WelcomePublishOutcome::BadSig;
    }
    if dht.store.persist_barrier().wait().await.is_err() {
        return WelcomePublishOutcome::BadSig;
    }
    common::debug!(
        "MLS welcome_publish: stored welcome for recipient={} id={}",
        recipient_short,
        hex::encode(id)
    );
    WelcomePublishOutcome::Stored
}

pub(crate) fn handle_welcome_fetch(
    dht: &Dht, req: WelcomeFetchReq, peer: NodeId, now_ms: u64,
) -> WelcomeFetchOutcome {
    let admit = || dht.welcome_limiters.check(&peer).is_ok();
    let stash = stash_prefix(&req.user_ipk.0);
    match stash_gate(dht, peer, Some(req.requester_relay_id), req.timestamp, now_ms, stash, admit) {
        Ok(()) => {},
        Err(Reject::RateLimited) => return WelcomeFetchOutcome::RateLimited,
        Err(Reject::NotOwner) => return WelcomeFetchOutcome::NotOwner,
        // `BadSig` reveals nothing about the queue.
        Err(Reject::Binding | Reject::Skew) => return WelcomeFetchOutcome::BadSig,
    }

    let msg = welcome_fetch_signing_input(
        MLS_WIRE_VERSION,
        &req.user_ipk.0,
        &req.requester_relay_id,
        req.timestamp,
    );
    if verify_ed25519(&req.user_ipk.0, &msg, &req.user_sig.0).is_err() {
        return WelcomeFetchOutcome::BadSig;
    }

    let entries = iterate_welcomes(dht, &req.user_ipk.0);
    let mut to_evict: Vec<[u8; STORAGE_KEY_LEN]> = Vec::new();
    let mut welcomes: Vec<WelcomeEntry> = Vec::with_capacity(entries.len());
    let mut used: usize = 0;
    for (key, env, expires_at_ms, stored_len) in entries {
        if expires_at_ms <= now_ms {
            to_evict.push(key);
            continue;
        }
        if welcomes.len() >= MAX_WELCOMES_PER_RECIPIENT
            || used.saturating_add(stored_len) > WELCOME_FETCH_MAX_BYTES
        {
            break;
        }
        let mut id = [0u8; WELCOME_ID_LEN];
        id.copy_from_slice(&key[STASH_PREFIX_LEN..]);
        used += stored_len;
        welcomes.push(WelcomeEntry {
            welcome_id: id.into(),
            envelope: env,
        });
    }

    for k in &to_evict {
        let _ = dht.store.welcome.remove(k);
    }

    WelcomeFetchOutcome::Found(WelcomeFetchFound { welcomes })
}

pub(crate) fn handle_welcome_ack(
    dht: &Dht, req: WelcomeAckReq, peer: NodeId, now_ms: u64,
) -> WelcomeAckResp {
    let admit = || dht.welcome_limiters.check(&peer).is_ok();
    let stash = stash_prefix(&req.user_ipk.0);
    if req.welcome_ids.len() > MAX_WELCOME_ACK_IDS
        || stash_gate(dht, peer, Some(req.requester_relay_id), req.timestamp, now_ms, stash, admit)
            .is_err()
    {
        return WelcomeAckResp { ok: false };
    }

    let ids: Vec<[u8; WELCOME_ID_LEN]> =
        req.welcome_ids.iter().map(|b| b.0).collect();
    let msg = welcome_ack_signing_input(
        MLS_WIRE_VERSION,
        &req.user_ipk.0,
        &req.requester_relay_id,
        &ids,
        req.timestamp,
    );
    if verify_ed25519(&req.user_ipk.0, &msg, &req.user_sig.0).is_err() {
        return WelcomeAckResp { ok: false };
    }

    // A failed delete answers `ok: false` so the recipient acks again instead of the row lingering.
    let mut all_ok = true;
    for id in &ids {
        let key = storage_key(&req.user_ipk.0, id);
        if let Err(e) = dht.store.welcome.remove(key) {
            common::warn!(
                "MLS welcome_ack: delete failed for ipk={} welcome_id={}: {e}",
                hex::encode(&req.user_ipk.0[..4]),
                hex::encode(id)
            );
            all_ok = false;
        }
    }

    if all_ok {
        common::debug!(
            "MLS welcome_ack: deleted {} welcome(s) for ipk={}",
            ids.len(),
            hex::encode(&req.user_ipk.0[..4])
        );
    }
    WelcomeAckResp { ok: all_ok }
}

#[cfg(test)]
mod tests {
    use common::utils::now_ms;
    use ed25519_dalek::Signer;

    use super::*;
    use crate::test_support::dht;
    use crate::test_support::key;
    use crate::test_support::welcome;

    /// The invite queue at a home: every Welcome is served, a republish reuses its row, the
    /// fetch and ack bind the relay that asks, and bad, stale, flooding or oversized requests
    /// change nothing.
    #[tokio::test]
    async fn a_welcome_queue_serves_and_deletes_only_for_its_signed_relay() {
        let (_dir, dht) = dht(NodeId::from_bytes([0; 32]));
        let (recipient, now) = (key(1), now_ms());
        let recipient_ipk = recipient.verifying_key().to_bytes();
        let (relay, replayer) = (NodeId::from_bytes([0xA1; 32]), NodeId::from_bytes([0xB2; 32]));
        let publish = |envelope, timestamp| {
            handle_welcome_publish(&dht, WelcomePublishReq { envelope, timestamp }, relay, now)
        };
        let fetch = |peer| {
            let msg = welcome_fetch_signing_input(MLS_WIRE_VERSION, &recipient_ipk, &relay, now);
            let req = WelcomeFetchReq {
                user_ipk:           recipient_ipk.into(),
                requester_relay_id: relay,
                timestamp:          now,
                user_sig:           recipient.sign(&msg).to_bytes().into(),
            };
            match handle_welcome_fetch(&dht, req, peer, now) {
                WelcomeFetchOutcome::Found(found) => {
                    Ok(found.welcomes.iter().map(|w| w.welcome_id.0).collect())
                },
                refused => Err(refused),
            }
        };
        let ack = |ids: Vec<[u8; WELCOME_ID_LEN]>, peer| {
            let msg =
                welcome_ack_signing_input(MLS_WIRE_VERSION, &recipient_ipk, &relay, &ids, now);
            let req = WelcomeAckReq {
                user_ipk:           recipient_ipk.into(),
                requester_relay_id: relay,
                welcome_ids:        ids.into_iter().map(Into::into).collect(),
                timestamp:          now,
                user_sig:           recipient.sign(&msg).to_bytes().into(),
            };
            handle_welcome_ack(&dht, req, peer, now).ok
        };

        let (first, second) =
            (welcome(&key(2), recipient_ipk, 1), welcome(&key(3), recipient_ipk, 2));
        let ids = [welcome_id(&first), welcome_id(&second)];
        let mut forged = first.clone();
        forged.sender_sig.0[0] ^= 1;
        assert_eq!(publish(forged, now).await, WelcomePublishOutcome::BadSig);
        assert_eq!(
            publish(first.clone(), now - 120_000).await,
            WelcomePublishOutcome::StaleTimestamp
        );
        assert_eq!(fetch(relay).map(|ids: Vec<_>| ids.len()), Ok(0));
        assert_eq!(publish(first.clone(), now).await, WelcomePublishOutcome::Stored);
        assert_eq!(publish(second, now).await, WelcomePublishOutcome::Stored);
        assert_eq!(
            publish(first, now).await,
            WelcomePublishOutcome::Stored,
            "a republish reuses its row"
        );
        let mut served: Vec<_> = fetch(relay).unwrap();
        served.sort();
        let mut expected = ids.to_vec();
        expected.sort();
        assert_eq!(served, expected);

        assert_eq!(
            fetch(replayer),
            Err(WelcomeFetchOutcome::BadSig),
            "another relay replays the fetch"
        );
        assert!(!ack(vec![ids[0]], replayer), "another relay replays the ack");
        assert!(!ack(vec![ids[0]; MAX_WELCOME_ACK_IDS + 1], relay));
        assert_eq!(fetch(relay).map(|ids: Vec<_>| ids.len()), Ok(2));
        assert!(ack(vec![ids[0]], relay));
        assert_eq!(fetch(relay), Ok(vec![ids[1]]));

        // The limiter clock stands still in tests, so the hourly budget cannot refill mid-burst.
        let refused = (0..=MAX_WELCOME_RPC_PER_HOUR).map(|_| fetch(relay)).find(Result::is_err);
        assert_eq!(refused, Some(Err(WelcomeFetchOutcome::RateLimited)));
    }
}
