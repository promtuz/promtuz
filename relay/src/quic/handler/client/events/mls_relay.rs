//! Tier-1 MLS wrapper handlers (libcore → home over `client/0`).
//! Each handler:
//!
//! 1. resolves the home's DHT (replies [`SRelayPacket::DhtUnavailable`]
//!    if this relay has DHT disabled);
//! 2. verifies the wrapper signature + ±60s skew against the
//!    connection-authenticated IPK (`ctx.ipk`) — for the three
//!    user-signed RPCs this *is* the inner Tier-2 user sig that the K
//!    storage homes will re-verify; for the two gate-only RPCs it's a
//!    local freshness/attribution gate;
//! 3. originates the real `peer/1` fan-out via `dht::mls_kp_originate` /
//!    `dht::mls_welcome_originate`;
//! 4. replies with the matching [`SRelayPacket`].
//!
//! On a failed verification we drop the stream (return `Ok(())` without
//! a reply); the phone's awaiting RPC surfaces a clean stream-closed
//! error. A correctly-behaving phone always signs validly, so this path
//! is only hit by a buggy or malicious client — we deliberately don't
//! leak a distinct "bad sig" reply.

use anyhow::Result;
use common::proto::Sender;
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
use common::quic::id::NodeId;
use common::trace;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use quinn::SendStream;

use crate::dht::mls::kp_originate;
use crate::dht::mls::welcome_originate;
use crate::quic::handler::client::ClientCtxHandle;
use crate::util::systime;

/// Shared skew + Ed25519-strict check against the connection IPK.
fn fresh_and_valid(ipk: &PublicKey, msg: &[u8], sig: &[u8; 64], now_ms: u64, timestamp: u64) -> bool {
    if now_ms.abs_diff(timestamp) > MAX_KP_SKEW_MS {
        return false;
    }
    ipk.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

// ---------------------------------------------------------------------------
// PublishKeyPackage (user-signed: inner kp_publish / kp_refill sig)
// ---------------------------------------------------------------------------

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
    let now_ms = systime().as_millis() as u64;
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_publish_keypackage(&ctx.ipk, now_ms, &records, mode, timestamp, &sig) {
        trace!("MLS publish-kp: wrapper sig/skew rejected");
        return Ok(());
    }
    let q = kp_originate::originate_publish(
        &dht, ctx.ipk.to_bytes(), records, mode, timestamp, sig, now_ms,
    )
    .await;
    SRelayPacket::KeyPackagePublished {
        homes_succeeded: q.homes_succeeded,
        quorum_met: q.quorum_met,
    }
    .send(tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FetchKeyPackage (gate-only: kp_fetch_wrap sig)
// ---------------------------------------------------------------------------

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
    let now_ms = systime().as_millis() as u64;
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_fetch_keypackage(&ctx.ipk, now_ms, &target_ipk, timestamp, &sig) {
        trace!("MLS fetch-kp: wrapper sig/skew rejected");
        return Ok(());
    }
    let r = kp_originate::originate_fetch(&dht, target_ipk, now_ms).await;
    SRelayPacket::KeyPackageFetched {
        record: r.record,
        remaining: r.remaining,
        static_hash: r.static_hash.into(),
    }
    .send(tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PublishWelcome (gate-only: welcome_publish_wrap sig; auth in envelope)
// ---------------------------------------------------------------------------

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
    let now_ms = systime().as_millis() as u64;
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_publish_welcome(&ctx.ipk, now_ms, &envelope, timestamp, &sig) {
        trace!("MLS publish-welcome: wrapper sig/skew rejected");
        return Ok(());
    }
    let quorum_met =
        welcome_originate::originate_welcome_publish(&dht, envelope, timestamp, now_ms).await;
    SRelayPacket::WelcomePublished { quorum_met }.send(tx).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FetchWelcomes (user-signed: inner welcome_fetch sig bound to our NodeId)
// ---------------------------------------------------------------------------

fn verify_fetch_welcomes(
    ipk: &PublicKey, node_id: &NodeId, now_ms: u64, timestamp: u64, sig: &[u8; 64],
) -> bool {
    let msg = welcome_fetch_signing_input(MLS_WIRE_VERSION, &ipk.to_bytes(), node_id, timestamp);
    fresh_and_valid(ipk, &msg, sig, now_ms, timestamp)
}

pub(crate) async fn handle_fetch_welcomes(
    ctx: ClientCtxHandle, timestamp: u64, sig: [u8; 64], tx: &mut SendStream,
) -> Result<()> {
    let now_ms = systime().as_millis() as u64;
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    if !verify_fetch_welcomes(&ctx.ipk, &dht.node_id, now_ms, timestamp, &sig) {
        trace!("MLS fetch-welcomes: wrapper sig/skew rejected");
        return Ok(());
    }
    let entries =
        welcome_originate::originate_welcome_fetch(&dht, ctx.ipk.to_bytes(), timestamp, sig, now_ms)
            .await;
    SRelayPacket::WelcomesFetched { entries }.send(tx).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// AckWelcomes (user-signed: inner welcome_ack sig bound to our NodeId)
// ---------------------------------------------------------------------------

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
    let now_ms = systime().as_millis() as u64;
    let Some(dht) = ctx.relay.dht.as_ref().cloned() else {
        SRelayPacket::DhtUnavailable.send(tx).await?;
        return Ok(());
    };
    let ids: Vec<[u8; 8]> = welcome_ids.iter().map(|b| b.0).collect();
    if !verify_ack_welcomes(&ctx.ipk, &dht.node_id, now_ms, &ids, timestamp, &sig) {
        trace!("MLS ack-welcomes: wrapper sig/skew rejected");
        return Ok(());
    }
    welcome_originate::originate_welcome_ack(&dht, ctx.ipk.to_bytes(), ids, timestamp, sig, now_ms)
        .await;
    SRelayPacket::WelcomesAcked.send(tx).await?;
    Ok(())
}
