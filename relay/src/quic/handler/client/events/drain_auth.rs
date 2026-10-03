//! `DrainAuth`: the user's signed permission for this relay to fetch its queue from the homes.
//! The transcript binds this relay's NodeId but not the home, so one signature serves every home.

use common::crypto::PublicKey;
use common::crypto::verify_ed25519;
use common::proto::dht_p2p::MAX_DHT_HELLO_SKEW_MS;
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::quic::id::NodeId;
use common::trace;
use common::utils::now_ms;

use crate::quic::handler::client::ClientCtxHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DrainAuth {
    pub timestamp: u64,
    pub sig: [u8; 64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainAuthError {
    StaleTimestamp,
    FutureTimestamp,
    BadSig,
}

pub(crate) fn verify_drain_auth(
    user_ipk: &PublicKey, relay_id: &NodeId, now_ms: u64, timestamp: u64, sig: [u8; 64],
) -> Result<DrainAuth, DrainAuthError> {
    if now_ms > timestamp && now_ms - timestamp > MAX_DHT_HELLO_SKEW_MS {
        return Err(DrainAuthError::StaleTimestamp);
    }
    if timestamp > now_ms && timestamp - now_ms > MAX_DHT_HELLO_SKEW_MS {
        return Err(DrainAuthError::FutureTimestamp);
    }

    // Strict: the pair is forwarded verbatim to every home, so a malleated encoding that
    // validated here would be rejected there.
    let transcript = queue_fetch_signing_input(user_ipk.as_bytes(), relay_id, timestamp);
    if verify_ed25519(user_ipk.as_bytes(), &transcript, &sig).is_err() {
        return Err(DrainAuthError::BadSig);
    }

    Ok(DrainAuth { timestamp, sig })
}

pub(crate) async fn handle_drain_auth(
    ctx: ClientCtxHandle, timestamp: u64, sig: [u8; 64],
) -> anyhow::Result<()> {
    let now_ms = now_ms();

    let Some(dht) = ctx.relay.dht.as_ref() else {
        trace!("DRAIN_AUTH: dropped (DHT disabled on this relay)");
        return Ok(());
    };

    let relay_id = dht.node_id;
    match verify_drain_auth(&ctx.ipk, &relay_id, now_ms, timestamp, sig) {
        Ok(auth) => {
            *ctx.drain_auth.lock() = Some(auth);
            trace!("DRAIN_AUTH: accepted (timestamp = {timestamp})");
        }
        Err(reason) => {
            trace!("DRAIN_AUTH: rejected — {reason:?}");
        }
    }
    Ok(())
}
