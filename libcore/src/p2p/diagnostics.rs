//! Bounded, process-local transmission evidence. No peer/file identifiers,
//! addresses, keys or payloads are retained here. Counters are cumulative;
//! take two snapshots to measure an experiment, not a production failure rate.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use once_cell::sync::Lazy;
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

static EVENTS: Lazy<Mutex<VecDeque<(u64, Event)>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static DIRECT_SENT: AtomicU64 = AtomicU64::new(0);
static RELAY_SENT: AtomicU64 = AtomicU64::new(0);
static CONTENT_SENT: AtomicU64 = AtomicU64::new(0);
static VERIFIED_RECEIVED: AtomicU64 = AtomicU64::new(0);
static TCP_QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record(event: Event) {
    let at = crate::utils::systime().as_millis() as u64;
    let mut events = EVENTS.lock();
    if events.len() == HISTORY_LIMIT {
        events.pop_front();
    }
    events.push_back((at, event));
    log::debug!("transmission: {event:?}");
}

/// QUIC datagram payload accepted by UDP or the bounded TCP relay queue. This includes
/// handshakes and retransmissions, excludes relay framing/IP overhead and
/// does NOT claim the remote relay delivered/billed these bytes. Setup packets
/// accepted on both relay paths count twice, including the bounded TCP race.
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
