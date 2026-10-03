//! MLS KeyPackage and Welcome RPCs: each is checked against the connection IPK, then originated
//! to the homes. A failed check gets no reply, so a bad signature has no distinct answer.

use anyhow::Result;
use common::proto::Sender;
use common::proto::mls_wire::KP_STASH_TARGET;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KpPublishMode;
use common::proto::mls_wire::MAX_KP_SKEW_MS;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::kp_fetch_wrap_signing_input;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_refill_signing_input;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::mls_wire::welcome_publish_wrap_signing_input;
use common::proto::client_rel::SRelayPacket;
use common::crypto::PublicKey;
use common::crypto::verify_ed25519;
use common::quic::id::NodeId;
use common::trace;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use quinn::SendStream;

use crate::dht::mls::kp_originate;
use crate::dht::mls::welcome_originate;
use crate::quic::handler::client::ClientCtxHandle;

fn fresh_and_valid(ipk: &PublicKey, msg: &[u8], sig: &[u8; 64], now_ms: u64, timestamp: u64) -> bool {
    if now_ms.abs_diff(timestamp) > MAX_KP_SKEW_MS {
        return false;
    }
    verify_ed25519(ipk.as_bytes(), msg, sig).is_ok()
}

fn verify_publish_keypackage(
    ipk: &PublicKey, now_ms: u64, records: &[KeyPackageRecord], mode: KpPublishMode,
    timestamp: u64, sig: &[u8; 64],
) -> bool {
    let ipk_bytes = ipk.to_bytes();
    let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
    let count = records.len() as u32;
    let msg = match mode {
        KpPublishMode::Publish => {
            kp_publish_signing_input(MLS_WIRE_VERSION, &ipk_bytes, &digest, count, timestamp)
        },
        KpPublishMode::Refill => {
            kp_refill_signing_input(MLS_WIRE_VERSION, &ipk_bytes, &digest, count, timestamp)
        },
    };
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_publish_keypackage(
    ctx: ClientCtxHandle, records: Vec<KeyPackageRecord>, timestamp: u64,
    mode: KpPublishMode, sig: [u8; 64], tx: &mut SendStream,
) -> Result<()> {
    // A batch the homes would reject anyway costs one length compare here
    // instead of a K-way reflection of the client's upload.
    if records.len() > KP_STASH_TARGET {
        trace!("MLS publish-kp: batch over KP_STASH_TARGET rejected");
        return Ok(());
    }
    let now_ms = now_ms();
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_publish_keypackage(&ctx.ipk, now_ms, &records, mode, timestamp, &sig) {
        trace!("MLS publish-kp: wrapper sig/skew rejected");
        return Ok(());
    }
    let q =
        kp_originate::originate_publish(&dht, ctx.ipk.to_bytes(), records, mode, timestamp, sig)
            .await;
    SRelayPacket::KeyPackagePublished {
        homes_succeeded: q.homes_succeeded,
        quorum_met: q.quorum_met,
    }
    .send(tx)
    .await?;
    Ok(())
}

/// This request may only inspect the connection owner's stash. The owner proof
/// is forwarded intact, bound to this relay's authenticated peer identity.
pub(crate) async fn handle_keypackage_inventory(
    ctx: ClientCtxHandle, bytes: &[u8], tx: &mut SendStream,
) -> Result<()> {
    use common::contracts::services::key_inventory::Request;
    let Ok(request) = Request::decode(bytes) else { return Ok(()); };
    if request.owner != ctx.ipk.to_bytes() { return Ok(()); }
    let Some(dht) = ctx.relay.dht.as_ref() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    let now = now_ms();
    let Some(inventory) = crate::dht::mls::inventory::originate(dht, &request, now).await else {
        return Ok(());
    };
    SRelayPacket::KeyPackageInventory { inventory: inventory.encode()?.into() }.send(tx).await?;
    Ok(())
}

fn verify_fetch_keypackage(
    ipk: &PublicKey, now_ms: u64, target_ipk: &[u8; 32], timestamp: u64, sig: &[u8; 64],
) -> bool {
    let msg = kp_fetch_wrap_signing_input(MLS_WIRE_VERSION, &ipk.to_bytes(), target_ipk, timestamp);
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_fetch_keypackage(
    ctx: ClientCtxHandle, target_ipk: [u8; 32], timestamp: u64, sig: [u8; 64],
    tx: &mut SendStream,
) -> Result<()> {
    let now_ms = now_ms();
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_fetch_keypackage(&ctx.ipk, now_ms, &target_ipk, timestamp, &sig) {
        trace!("MLS fetch-kp: wrapper sig/skew rejected");
        return Ok(());
    }
    // The home's quota is keyed on this relay, so without a per-client gate one
    // client drains every co-tenant's budget against the same target.
    if ctx.limits.fetch_keypackage.check_key(&target_ipk).is_err() {
        trace!("MLS fetch-kp: per-client quota for this target exhausted");
        return Ok(());
    }
    let r = kp_originate::originate_fetch(&dht, target_ipk, now_ms).await;
    if r.unavailable {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    }
    SRelayPacket::KeyPackageFetched {
        record: r.record,
        remaining: r.remaining,
        static_hash: r.static_hash.into(),
    }
    .send(tx)
    .await?;
    Ok(())
}

fn verify_publish_welcome(
    ipk: &PublicKey, now_ms: u64, envelope: &WelcomeEnvelopeP, timestamp: u64, sig: &[u8; 64],
) -> bool {
    let msg = welcome_publish_wrap_signing_input(
        MLS_WIRE_VERSION, &ipk.to_bytes(), &envelope.welcome_blob.0, timestamp,
    );
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_publish_welcome(
    ctx: ClientCtxHandle, envelope: WelcomeEnvelopeP, timestamp: u64, sig: [u8; 64],
    tx: &mut SendStream,
) -> Result<()> {
    // The wrapper sig shows only that some client asked; this binding makes it the author, so a
    // captured envelope cannot be replayed to fill the recipient's welcome queue.
    if envelope.sender_ipk.0 != ctx.ipk.to_bytes() {
        trace!("MLS publish-welcome: envelope sender is not the publishing client");
        return Ok(());
    }
    let now_ms = now_ms();
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_publish_welcome(&ctx.ipk, now_ms, &envelope, timestamp, &sig) {
        trace!("MLS publish-welcome: wrapper sig/skew rejected");
        return Ok(());
    }
    let quorum_met = welcome_originate::originate_welcome_publish(&dht, envelope, timestamp).await;
    SRelayPacket::WelcomePublished { quorum_met }.send(tx).await?;
    Ok(())
}

fn verify_fetch_welcomes(
    ipk: &PublicKey, node_id: &NodeId, now_ms: u64, timestamp: u64, sig: &[u8; 64],
) -> bool {
    let msg = welcome_fetch_signing_input(MLS_WIRE_VERSION, &ipk.to_bytes(), node_id, timestamp);
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_fetch_welcomes(
    ctx: ClientCtxHandle, timestamp: u64, sig: [u8; 64], tx: &mut SendStream,
) -> Result<()> {
    let now_ms = now_ms();
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_fetch_welcomes(&ctx.ipk, &dht.node_id, now_ms, timestamp, &sig) {
        trace!("MLS fetch-welcomes: wrapper sig/skew rejected");
        return Ok(());
    }
    let entries =
        welcome_originate::originate_welcome_fetch(&dht, ctx.ipk.to_bytes(), timestamp, sig).await;
    SRelayPacket::WelcomesFetched { entries }.send(tx).await?;
    Ok(())
}

fn verify_ack_welcomes(
    ipk: &PublicKey, node_id: &NodeId, now_ms: u64, ids: &[[u8; 8]], timestamp: u64,
    sig: &[u8; 64],
) -> bool {
    let msg = welcome_ack_signing_input(MLS_WIRE_VERSION, &ipk.to_bytes(), node_id, ids, timestamp);
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_ack_welcomes(
    ctx: ClientCtxHandle, welcome_ids: Vec<Bytes<8>>, timestamp: u64, sig: [u8; 64],
    tx: &mut SendStream,
) -> Result<()> {
    let now_ms = now_ms();
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    let ids: Vec<[u8; 8]> = welcome_ids.iter().map(|b| b.0).collect();
    if !verify_ack_welcomes(&ctx.ipk, &dht.node_id, now_ms, &ids, timestamp, &sig) {
        trace!("MLS ack-welcomes: wrapper sig/skew rejected");
        return Ok(());
    }
    welcome_originate::originate_welcome_ack(&dht, ctx.ipk.to_bytes(), ids, timestamp, sig).await;
    SRelayPacket::WelcomesAcked.send(tx).await?;
    Ok(())
}
