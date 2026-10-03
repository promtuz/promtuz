//! Process-local transmission counters and a bounded event history. Nothing here holds
//! identifiers, addresses, keys or payloads.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::LazyLock;
use common::utils::now_ms;
use parking_lot::Mutex;

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

static EVENTS: LazyLock<Mutex<VecDeque<(u64, Event)>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));
static DIRECT_SENT: AtomicU64 = AtomicU64::new(0);
static RELAY_SENT: AtomicU64 = AtomicU64::new(0);
static CONTENT_SENT: AtomicU64 = AtomicU64::new(0);
static VERIFIED_RECEIVED: AtomicU64 = AtomicU64::new(0);
static TCP_QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record(event: Event) {
    let at = now_ms();
    let mut events = EVENTS.lock();
    if events.len() == HISTORY_LIMIT {
        events.pop_front();
    }
    events.push_back((at, event));
    log::debug!("transmission: {event:?}");
}

/// QUIC datagram bytes accepted by UDP or the TCP relay queue, not bytes delivered. Setup
/// packets sent on both relay paths count twice.
pub(super) fn sent_datagram(relayed: bool, bytes: usize) {
    let counter = if relayed { &RELAY_SENT } else { &DIRECT_SENT };
    counter.fetch_add(bytes as u64, Ordering::Relaxed);
}

pub(crate) fn sent_content(bytes: u64) {
    CONTENT_SENT.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn received_verified(bytes: u64) {
    VERIFIED_RECEIVED.fetch_add(bytes, Ordering::Relaxed);
}

pub(super) fn dropped_tcp_datagram() {
    TCP_QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
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
    Snapshot {
        events: EVENTS.lock().iter().copied().collect(),
        direct_sent: DIRECT_SENT.load(Ordering::Relaxed),
        relay_sent: RELAY_SENT.load(Ordering::Relaxed),
        content_sent: CONTENT_SENT.load(Ordering::Relaxed),
        verified_received: VERIFIED_RECEIVED.load(Ordering::Relaxed),
        tcp_queue_drops: TCP_QUEUE_DROPS.load(Ordering::Relaxed),
    }
}
