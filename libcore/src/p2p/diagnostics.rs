//! Process-local transmission counters and a bounded event history. Nothing here holds
//! identifiers, addresses, keys or payloads.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use common::utils::now_ms;
use parking_lot::Mutex;

use crate::state::core;

const HISTORY_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    NetworkChanged,
    SignalingFailed,
    MissingCandidates,
    RelayOnlyPolicy,
    PunchTimeout,
    DirectReady,
    LinkReady,
    DirectLost,
    RelayLost,
    TcpRelayReady,
    TcpRelayLost,
    TcpRelayUnavailable,
    HandshakeFailed,
    VerificationFailed,
    TransportRetry,
    AuthenticationFailed,
    ValidationFailed,
    StorageFailed,
    Unavailable,
    RetryExhausted,
    TransferComplete,
}

#[derive(Default)]
pub(super) struct Diagnostics {
    events:            Mutex<VecDeque<(u64, Event)>>,
    direct_sent:       AtomicU64,
    relay_sent:        AtomicU64,
    content_sent:      AtomicU64,
    verified_received: AtomicU64,
    tcp_queue_drops:   AtomicU64,
}

pub(crate) fn record(event: Event) {
    let at = now_ms();
    let mut events = core().p2p.diagnostics.events.lock();
    if events.len() == HISTORY_LIMIT {
        events.pop_front();
    }
    events.push_back((at, event));
    log::debug!("transmission: {event:?}");
}

/// QUIC datagram bytes accepted by UDP or the TCP relay queue, not bytes delivered. Setup
/// packets sent on both relay paths count twice.
pub(super) fn sent_datagram(relayed: bool, bytes: usize) {
    let d = &core().p2p.diagnostics;
    let counter = if relayed { &d.relay_sent } else { &d.direct_sent };
    counter.fetch_add(bytes as u64, Ordering::Relaxed);
}

pub(crate) fn sent_content(bytes: u64) {
    core().p2p.diagnostics.content_sent.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn received_verified(bytes: u64) {
    core().p2p.diagnostics.verified_received.fetch_add(bytes, Ordering::Relaxed);
}

pub(super) fn dropped_tcp_datagram() {
    core().p2p.diagnostics.tcp_queue_drops.fetch_add(1, Ordering::Relaxed);
}

pub(crate) struct Snapshot {
    pub events: Vec<(u64, Event)>,
    pub direct_sent: u64,
    pub relay_sent: u64,
    pub content_sent: u64,
    pub verified_received: u64,
    pub tcp_queue_drops: u64,
}

pub(crate) fn snapshot() -> Snapshot {
    let d = &core().p2p.diagnostics;
    Snapshot {
        events:            d.events.lock().iter().copied().collect(),
        direct_sent:       d.direct_sent.load(Ordering::Relaxed),
        relay_sent:        d.relay_sent.load(Ordering::Relaxed),
        content_sent:      d.content_sent.load(Ordering::Relaxed),
        verified_received: d.verified_received.load(Ordering::Relaxed),
        tcp_queue_drops:   d.tcp_queue_drops.load(Ordering::Relaxed),
    }
}
