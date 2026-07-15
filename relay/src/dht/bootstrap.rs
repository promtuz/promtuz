//! Cold-start bootstrap: ask the resolver for seed peers, seed the
//! routing table, then run a self-`FindNode` for forced convergence.
//!
//! Three steps: **A** query the resolver's `GetBootstrapPeers`; **B**
//! insert the returned descriptors into the routing table; **C** run an
//! iterative `FindNode` on our own id so every peer the walk touches learns
//! to route back to us (the resolver's seed set only tells *us* about
//! *them*, not the reverse). The scheduler in `dht/sync` re-runs the whole
//! sequence on a backoff while the routing table is sparse.
//!
//! ## Failure semantics
//!
//! Bootstrap failure is **not fatal**. The relay keeps serving QUIC
//! traffic even with an empty routing table — DHT RPCs simply have no
//! peers to forward to until the next bootstrap attempt succeeds. The
//! caller is expected to log [`BootstrapError`] and either retry on a
//! schedule or accept the empty-routing-table state.
//!
//! ## Concurrency
//!
//! The whole machine takes only `Arc<Dht>` and a [`ResolverLinkHandle`]
//! — no `&Relay` reference — so it's safe to drive from a detached
//! `tokio::spawn` task. `parking_lot` guards on `dht.routing` are
//! never held across an `await`: we batch the inserts into a single
//! write-guard scope after the resolver round-trip has fully completed.

use std::sync::Arc;

use common::proto::client_res::RelayDescriptor;
use common::proto::dht_p2p::NodeDescriptor;
use thiserror::Error;

use super::Dht;
use super::routing::InsertOutcome;
use crate::quic::resolver_link::ResolverLinkHandle;

/// Phases of the bootstrap state machine. The implementation is a
/// `match`-driven step machine.
///
/// The variant is threaded through return values rather than stored on
/// [`Dht`]; the scheduler that re-runs bootstrap reads the outcome, not a
/// persistent state field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapState {
    /// Initial: routing table empty, no resolver query in flight.
    Cold,

    /// Resolver returned descriptors; we're inserting them into the
    /// routing table. (Populating `rtt_ema_ms` with speculative `PING`s to
    /// each is future work.)
    Warming,

    /// Routing table has enough peers to start answering RPCs; the
    /// self-FindNode walk is in flight.
    Walking,

    /// Bootstrap complete; relay can serve `FindValue`/`Store` for
    /// itself.
    Ready,
}

/// Counts asked of the resolver in the phase-A query. Pulled out as
/// constants so a follow-up can adjust them without code changes. We use
/// 8 XOR-near + 4 RTT-near to weight the routing-table-shape signal
/// slightly above the liveness-recency signal.
const BOOTSTRAP_COUNT_XOR_NEAR: u8 = 8;
const BOOTSTRAP_COUNT_RTT_NEAR: u8 = 4;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Reasons [`bootstrap`] can fail.
///
/// All variants are non-fatal: the caller should log and continue. The
/// relay can still answer client traffic with an empty routing table —
/// it just can't forward DHT lookups until a future bootstrap attempt
/// succeeds.
#[derive(Debug, Error)]
pub enum BootstrapError {
    /// The resolver session is not currently live. Either the resolver
    /// link hasn't connected yet (race at startup) or it's reconnecting
    /// after a transient disconnect. The scheduler retries on a backoff.
    #[error("bootstrap: no live resolver session for GetBootstrapPeers")]
    ResolverUnavailable,

    /// Resolver reachable but returned zero descriptors in *both* lists.
    /// Legitimate on a brand-new network with one relay (this one); not
    /// a bug. Logged at info-level upstream.
    #[error("bootstrap: resolver returned an empty registry")]
    EmptyRegistry,

    /// Wire-format issue talking to the resolver — packing the request,
    /// stream errors, decoding the response, or a non-`GetBootstrapPeers`
    /// reply. Wraps `anyhow::Error` because the underlying
    /// `ResolverLinkHandle::get_bootstrap_peers` already aggregates
    /// quinn / postcard errors that way.
    #[error("bootstrap: wire/codec error talking to resolver: {0}")]
    Decode(#[source] anyhow::Error),
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the bootstrap state machine end-to-end: phase A (resolver query),
/// phase B (routing-table insert), and phase C (self-FindNode convergence).
/// The scheduler in `dht/sync` adds the periodic retry loop.
///
/// This function never panics. Errors are returned for the caller to
/// log; the relay is expected to keep running either way.
///
/// Takes `Arc<Dht>` (not `&Dht`) so it can be spawned via
/// `tokio::spawn` from `Relay::new` without lifetime gymnastics, and a
/// [`ResolverLinkHandle`] that is cheap to clone alongside the spawn.
pub async fn bootstrap(
    dht: Arc<Dht>, resolver: ResolverLinkHandle,
) -> Result<BootstrapState, BootstrapError> {
    // ---- Phase A: resolver query ([Cold] -> [Warming]) ---------------
    crate::dht_log!(
        "DHT bootstrap: querying resolver for {} XOR-near + {} RTT-near peers",
        BOOTSTRAP_COUNT_XOR_NEAR, BOOTSTRAP_COUNT_RTT_NEAR
    );

    let near = *dht.node_id.as_bytes();
    let (xor_near, rtt_near) = resolver
        .get_bootstrap_peers(near, BOOTSTRAP_COUNT_XOR_NEAR, BOOTSTRAP_COUNT_RTT_NEAR)
        .await
        .map_err(classify_handle_error)?;

    if xor_near.is_empty() && rtt_near.is_empty() {
        // Brand-new network with no peers to seed from. Not fatal — the
        // relay simply can't forward DHT lookups until something else
        // populates the routing table (e.g. an incoming peer learns of
        // us via a separate path).
        return Err(BootstrapError::EmptyRegistry);
    }

    // ---- Phase B: insert into routing table ([Warming]) --------------
    //
    // De-duplicate by `id` across the two lists *before* taking the
    // routing-table write lock — the resolver intentionally doesn't
    // dedupe (so callers see how each ranking voted), but we want one
    // insert per peer to keep the routing-table semantics clean
    // (`InsertOutcome::Refreshed` for the second copy is harmless but
    // noisy).
    let mut seen = std::collections::HashSet::with_capacity(xor_near.len() + rtt_near.len());
    let mut to_insert: Vec<NodeDescriptor> = Vec::with_capacity(seen.capacity());
    for rd in xor_near.iter().chain(rtt_near.iter()) {
        if seen.insert(rd.id) {
            to_insert.push(node_descriptor_from(rd));
        }
    }

    // Single write-guard scope — never held across `await` (the only
    // `await` ahead of this is the resolver round-trip, already done).
    let mut inserted = 0usize;
    let mut refreshed = 0usize;
    let mut deferred = 0usize;
    let mut self_skipped = 0usize;
    {
        let mut routing = dht.routing.write();
        for desc in to_insert {
            match routing.insert(desc) {
                InsertOutcome::Inserted => inserted += 1,
                InsertOutcome::Refreshed => refreshed += 1,
                InsertOutcome::PendingPing(_) | InsertOutcome::Discarded => deferred += 1,
                InsertOutcome::IsSelf => self_skipped += 1,
            }
        }
    }

    crate::dht_log!(
        "DHT bootstrap: inserted={}, refreshed={}, deferred={}, self={} (xor_near={}, rtt_near={})",
        inserted,
        refreshed,
        deferred,
        self_skipped,
        xor_near.len(),
        rtt_near.len()
    );

    // ---- Phase C: self-FindNode lookup ([Walking]) -------------------
    // Force convergence with an iterative `FindNode` on our own id. Each
    // peer the walk dials receives our signed `DhtHello` and records us in
    // its routing table — this is the *only* way the rest of the network
    // learns to reach us, since the resolver's near-set seeds our table
    // but not theirs. It also pulls in any peers the resolver didn't vend.
    //
    // Best-effort: a relay may answer RPCs with imperfect routing (α=3
    // hedging compensates), so a failed walk — e.g. the very first cold
    // call before the dialer endpoint is attached — logs and proceeds. The
    // scheduler re-runs it on a backoff, and cached peer connections make
    // the repeats cheap.
    match crate::dht::lookup::lookup_node(dht.clone(), dht.node_id).await {
        Ok(peers) => crate::dht_log!("DHT bootstrap: self-FindNode converged on {} peer(s)", peers.len()),
        Err(e) => crate::dht_log!("DHT bootstrap: self-FindNode walk failed: {e}; proceeding with seeded routing"),
    }

    // ---- Phase D: mark ready ([Ready]) -------------------------------
    crate::dht_log!("DHT bootstrap: complete (resolver seed + self-FindNode convergence)");
    Ok(BootstrapState::Ready)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Classify the `anyhow::Error` returned by
/// [`ResolverLinkHandle::get_bootstrap_peers`] into the typed
/// [`BootstrapError`] variants.
///
/// We pattern on the textual message rather than a downcasted error type
/// because the handle synthesises the "no live session" path itself
/// (with a fresh `anyhow!` from `Context::context`); the wire path
/// surfaces quinn / postcard errors that don't have a stable
/// taxonomy. The textual match is robust because the only string we
/// generate ourselves is the "no live resolver session" one — anything
/// else is a wire issue and falls through to `Decode`.
fn classify_handle_error(err: anyhow::Error) -> BootstrapError {
    let msg = err.to_string();
    if msg.contains("no live resolver session") {
        BootstrapError::ResolverUnavailable
    } else {
        BootstrapError::Decode(err)
    }
}

/// Convert the resolver's [`RelayDescriptor`] into a
/// [`NodeDescriptor`] suitable for [`super::routing::RoutingTable::insert`].
///
/// The two types carry the same information but live in different proto
/// modules — `RelayDescriptor` is the resolver-facing shape (used in
/// `client_res.rs`), `NodeDescriptor` is the relay-to-relay DHT shape
/// (used in `dht_p2p.rs`). The fields are point-by-point equivalent.
fn node_descriptor_from(rd: &RelayDescriptor) -> NodeDescriptor {
    NodeDescriptor { id: rd.id, addr: rd.addr, pubkey: rd.pubkey }
}

// `BootstrapState::Cold`, `Warming`, `Walking` have no consumers yet —
// they only become live once the scheduler reads the state off `Dht`
// rather than the bootstrap return value. The crate-wide
// `#![allow(dead_code)]` on `dht/mod.rs` covers this; no per-item
// suppression needed.
