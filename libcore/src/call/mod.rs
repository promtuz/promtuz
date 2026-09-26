//! Calls (CALLS.md). One call per device, driven by a state machine that
//! owns every timer; the media runs in a [`rtc`] session on str0m, the
//! audio crosses the FFI through [`audio`]. Signaling rides the MLS channel
//! as [`CallMsg`], so it is end-to-end and needs no key exchange of its own.
//!
//! Roles are fixed at the offer: the caller controls ICE and answers DTLS
//! passively, the callee the reverse. When both tap call at once, the lower
//! identity key's offer wins and the other side answers it instead of
//! ringing, the tie-break the P2P dial already uses.

pub(crate) mod audio;
pub mod ffi;
mod rtc;

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::Wake;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::CallCandidate;
use common::proto::mls_wire::CallEnd;
use common::proto::mls_wire::CallMsg;
use common::proto::pack::Unpacker;
use common::proto::Sender as _;
use log::debug;
use log::info;
use log::warn;
use parking_lot::Mutex;
use str0m::crypto::dtls::DtlsCert;
use tokio::sync::mpsc;

use crate::RUNTIME;
use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::db::messages::SYSTEM_CALL;
use crate::platform::CallEndReason;
use crate::platform::CallEvent;
use crate::state::RELAY;
use audio::AudioPath;

/// Life of every call signal at the relay. An offer is worth nothing after
/// the ring, and the rest of a call is worth nothing after the offer.
pub(crate) const SIGNAL_TTL_MS: u64 = 40_000;
/// The offer's own expiry, on the caller's clock.
const OFFER_LIFE: Duration = Duration::from_secs(40);
/// Tolerated clock difference between the two phones judging an expiry.
const CLOCK_SKEW_MS: u64 = 5_000;
/// How long a call rings before the caller gives up and the callee misses it.
const RING_TIMEOUT: Duration = Duration::from_secs(45);
/// From answer to media, and from a dropped path to a recovered one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Asking the relay for TURN credentials must not hold up the ring.
const TURN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// We rang them and wait for an answer.
    Offering,
    /// They rang us and wait for ours.
    Ringing,
    /// Answered; ICE and DTLS under way.
    Connecting,
    Connected,
    /// The path dropped; a restart is under way.
    Reconnecting,
}

/// The peer's half of the session parameters.
#[derive(Clone)]
struct Remote {
    ufrag:       String,
    pwd:         String,
    fingerprint: [u8; 32],
    ssrc:        u32,
    video_ssrc:  u32,
    candidates:  Vec<CallCandidate>,
}

struct Call {
    id:           [u8; 16],
    peer:         [u8; 32],
    conversation: [u8; 16],
    outgoing:     bool,
    video:        bool,
    phase:        Phase,
    connected_at: Option<Instant>,
    peer_muted:   bool,
    audio:        Arc<AudioPath>,
    /// Kept for the call's life so a restarted session keeps its fingerprint.
    cert:         DtlsCert,
    session:      Option<mpsc::UnboundedSender<rtc::Cmd>>,
    /// The peer's parameters, held until a session exists to take them.
    remote:       Option<Remote>,
    /// Candidates that arrived before the session did.
    early:        Vec<CallCandidate>,
    /// Restarts so far, ours or theirs; see [`CallMsg::Restart`].
    restart_gen:  u32,
}

/// A read-only view for the platform.
pub struct Snapshot {
    pub id:           [u8; 16],
    pub peer:         [u8; 32],
    pub conversation: [u8; 16],
    pub outgoing:     bool,
    pub video:        bool,
    pub phase:        Phase,
    pub muted:        bool,
    pub peer_muted:   bool,
    pub connected_ms: u64,
}

static CURRENT: Mutex<Option<Call>> = parking_lot::const_mutex(None);

fn now_ms() -> u64 {
    crate::utils::systime().as_millis() as u64
}

fn rand_id() -> [u8; 16] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b
}

fn emit(event: CallEvent) {
    if let Some(events) = crate::platform::EVENTS.get() {
        events.on_call(event);
    }
}

fn short(id: &[u8]) -> String {
    hex::encode(&id[..4])
}

/// Ring `peer`, as a video call when `video`. Returns the call id; the rest
/// arrives as events.
pub fn start(peer: [u8; 32], video: bool) -> Result<[u8; 16]> {
    if !Contact::is_paired(&peer) {
        bail!("not a paired contact");
    }
    let conversation = Conversation::for_peer(&peer)?;
    let cert = new_cert()?;
    let id = rand_id();
    {
        let mut current = CURRENT.lock();
        if current.is_some() {
            bail!("already in a call");
        }
        *current = Some(Call {
            id,
            peer,
            conversation,
            outgoing: true,
            video,
            phase: Phase::Offering,
            connected_at: None,
            peer_muted: false,
            audio: Arc::new(AudioPath::new()?),
            cert,
            session: None,
            remote: None,
            early: Vec::new(),
            restart_gen: 0,
        });
    }
    info!("CALL[{}]: calling {} ({})", short(&id), short(&peer), if video { "video" } else { "audio" });
    emit(CallEvent::Outgoing { call: id.to_vec(), peer: peer.to_vec(), conversation: conversation.to_vec() });
    RUNTIME.spawn(spawn_session(id, rtc::Role::Caller, video));
    arm(RING_TIMEOUT, id, Phase::Offering, |call| {
        info!("CALL[{}]: no answer", short(&call.id));
        signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Unanswered });
        Some(CallEndReason::Unanswered)
    });
    Ok(id)
}

/// Pick up the call that is ringing.
pub fn accept() -> Result<()> {
    let (id, video) = {
        let mut current = CURRENT.lock();
        let call = current.as_mut().ok_or_else(|| anyhow!("no call"))?;
        if call.phase != Phase::Ringing {
            bail!("nothing to accept");
        }
        call.phase = Phase::Connecting;
        (call.id, call.video)
    };
    info!("CALL[{}]: accepted", short(&id));
    emit(CallEvent::Connecting { call: id.to_vec() });
    RUNTIME.spawn(spawn_session(id, rtc::Role::Callee, video));
    arm(CONNECT_TIMEOUT, id, Phase::Connecting, |call| {
        signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Failed });
        Some(CallEndReason::Failed)
    });
    Ok(())
}

/// Refuse the call that is ringing.
pub fn reject() {
    let ended = {
        let mut current = CURRENT.lock();
        current.as_mut().filter(|c| c.phase == Phase::Ringing).map(|call| {
            signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Declined });
            (call.id, CallEndReason::Declined)
        })
    };
    if let Some((id, reason)) = ended {
        end(id, reason);
    }
}

/// Hang up whatever is going on: cancel a ring, refuse a ring, or end a call.
pub fn hangup() {
    let ended = {
        let mut current = CURRENT.lock();
        current.as_mut().map(|call| {
            let reason = match call.phase {
                Phase::Offering => {
                    signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Hangup });
                    CallEndReason::Cancelled
                },
                Phase::Ringing => {
                    signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Declined });
                    CallEndReason::Declined
                },
                _ => {
                    signal(call.conversation, CallMsg::End { call: call.id, reason: CallEnd::Hangup });
                    CallEndReason::Hangup
                },
            };
            (call.id, reason)
        })
    };
    if let Some((id, reason)) = ended {
        end(id, reason);
    }
}

pub fn set_muted(muted: bool) {
    with_call(|call| {
        call.audio.set_muted(muted);
        if matches!(call.phase, Phase::Connected | Phase::Reconnecting) {
            signal(call.conversation, CallMsg::Media { call: call.id, muted });
        }
        None
    });
}

/// The platform's default network moved (wifi to cellular, or back). Start
/// over on the new addresses now instead of waiting for ICE to notice.
pub fn network_changed() {
    with_call(|call| {
        if matches!(call.phase, Phase::Connected | Phase::Reconnecting) {
            begin_restart(call, None);
        }
        None
    });
}

pub fn current() -> Option<Snapshot> {
    CURRENT.lock().as_ref().map(|c| Snapshot {
        id:           c.id,
        peer:         c.peer,
        conversation: c.conversation,
        outgoing:     c.outgoing,
        video:        c.video,
        phase:        c.phase,
        muted:        c.audio.muted(),
        peer_muted:   c.peer_muted,
        connected_ms: c.connected_at.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0),
    })
}

/// Turn our camera on or off in a video call. The camera device is the
/// platform's; core only tells the peer, so their screen shows our video or
/// our avatar.
pub fn set_camera(on: bool) {
    with_call(|call| {
        if call.video && matches!(call.phase, Phase::Connected | Phase::Reconnecting) {
            signal(call.conversation, CallMsg::Camera { call: call.id, on });
        }
        None
    });
}

/// One encoded H.264 access unit (Annex-B) from the platform's video encoder.
/// The keyframe flag is the platform's to know; str0m re-derives it, so it is
/// not threaded through.
pub fn video_capture(frame: Vec<u8>, _keyframe: bool) {
    let session = CURRENT.lock().as_ref().and_then(|c| c.session.clone());
    if let Some(session) = session {
        let _ = session.send(rtc::Cmd::Video(frame));
    }
}

/// One captured 20 ms frame of 48 kHz mono PCM, little-endian.
pub fn audio_capture(pcm: &[u8]) {
    let target = {
        let current = CURRENT.lock();
        current.as_ref().and_then(|c| Some((c.audio.clone(), c.session.clone()?)))
    };
    if let Some((audio, session)) = target {
        if let Some(packet) = audio.encode(pcm) {
            let _ = session.send(rtc::Cmd::Audio(packet));
        }
    }
}

/// The next `frames` of playback, or silence outside a call.
pub fn audio_playback(frames: usize) -> Vec<u8> {
    let audio = CURRENT.lock().as_ref().map(|c| c.audio.clone());
    match audio {
        Some(audio) => audio.playback(frames),
        None => vec![0u8; frames * audio::FRAME_SAMPLES * 2],
    }
}

/// Inbound call signaling, from the direct chat with `from`.
pub(crate) fn on_signal(from: [u8; 32], conversation: [u8; 16], msg: CallMsg) {
    match msg {
        CallMsg::Offer { call, expires_at_ms, video, ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates } => {
            let remote = Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates };
            on_offer(from, conversation, call, expires_at_ms, video, remote);
        },
        CallMsg::Ringing { call } => {
            with_call(|c| {
                if c.id == call && c.phase == Phase::Offering {
                    emit(CallEvent::Ringing { call: call.to_vec() });
                }
                None
            });
        },
        CallMsg::Answer { call, ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates } => {
            with_call(|c| {
                if c.id != call || c.phase != Phase::Offering {
                    return None;
                }
                info!("CALL[{}]: answered", short(&call));
                c.phase = Phase::Connecting;
                let remote = Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates };
                offer_remote(c, remote);
                emit(CallEvent::Connecting { call: call.to_vec() });
                None
            });
            arm(CONNECT_TIMEOUT, call, Phase::Connecting, |c| {
                signal(c.conversation, CallMsg::End { call: c.id, reason: CallEnd::Failed });
                Some(CallEndReason::Failed)
            });
        },
        CallMsg::Candidate { call, candidate } => {
            with_call(|c| {
                if c.id == call {
                    match &c.session {
                        Some(s) => {
                            let _ = s.send(rtc::Cmd::Candidate(candidate));
                        },
                        None => c.early.push(candidate),
                    }
                }
                None
            });
        },
        CallMsg::Media { call, muted } => {
            with_call(|c| {
                if c.id == call {
                    c.peer_muted = muted;
                    emit(CallEvent::PeerMuted { call: call.to_vec(), muted });
                }
                None
            });
        },
        CallMsg::Camera { call, on } => {
            with_call(|c| {
                if c.id == call {
                    emit(CallEvent::PeerCamera { call: call.to_vec(), on });
                }
                None
            });
        },
        CallMsg::Restart { call, generation, ufrag, pwd, candidates } => {
            with_call(|c| {
                if c.id != call || !matches!(c.phase, Phase::Connected | Phase::Reconnecting) {
                    return None;
                }
                let fingerprint = c.remote.as_ref().map(|r| r.fingerprint).unwrap_or_default();
                let ssrc = c.remote.as_ref().map(|r| r.ssrc).unwrap_or_default();
                let video_ssrc = c.remote.as_ref().map(|r| r.video_ssrc).unwrap_or_default();
                let remote = Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates };
                if generation > c.restart_gen {
                    // They lost the path first; start over on our side too and
                    // answer with fresh credentials once we have them.
                    c.restart_gen = generation;
                    begin_restart(c, Some(remote));
                } else {
                    // Their answer to our restart, or the same round of a
                    // shared drop: the fresh session takes their side now.
                    offer_remote(c, remote);
                }
                None
            });
        },
        CallMsg::End { call, reason } => {
            let ended = with_call(|c| {
                if c.id != call {
                    return None;
                }
                let ours = match (reason, c.phase) {
                    (CallEnd::Busy, _) => CallEndReason::Busy,
                    (CallEnd::Declined, _) => CallEndReason::Declined,
                    (CallEnd::Failed, _) => CallEndReason::Failed,
                    // Before we picked up, their hangup or timeout is our miss.
                    (CallEnd::Hangup | CallEnd::Unanswered, Phase::Ringing) => CallEndReason::Missed,
                    (CallEnd::Unanswered, _) => CallEndReason::Unanswered,
                    (CallEnd::Hangup, Phase::Offering | Phase::Connecting) => CallEndReason::Cancelled,
                    (CallEnd::Hangup, _) => CallEndReason::Hangup,
                };
                Some(ours)
            });
            if let Some(reason) = ended {
                info!("CALL[{}]: ended by peer: {reason:?}", short(&call));
                end(call, reason);
            }
        },
    }
}

fn on_offer(
    from: [u8; 32], conversation: [u8; 16], call: [u8; 16], expires_at_ms: u64, video: bool,
    remote: Remote,
) {
    if !Contact::is_paired(&from) {
        debug!("CALL[{}]: offer from a stranger ignored", short(&call));
        return;
    }
    if now_ms() > expires_at_ms.saturating_add(CLOCK_SKEW_MS) {
        // Reached us after it stopped ringing anywhere: a missed call, not a
        // ring. The caller has already recorded no answer.
        info!("CALL[{}]: offer from {} expired in transit", short(&call), short(&from));
        record(conversation, from, &call, "missed", false);
        emit(CallEvent::Ended {
            call: call.to_vec(),
            peer: from.to_vec(),
            conversation: conversation.to_vec(),
            reason: CallEndReason::Missed,
            duration_ms: 0,
        });
        return;
    }
    let me = Identity::get().map(|i| i.ipk()).unwrap_or([0xff; 32]);
    let mut current = CURRENT.lock();
    match current.as_mut() {
        None => {},
        Some(c) if c.id == call => return,
        // Both tapped call at once. The lower key's offer is the call; the
        // other side treats it as the answer to its own and picks up.
        Some(c) if c.phase == Phase::Offering && c.peer == from => {
            if from < me {
                info!("CALL[{}]: crossed offers, taking theirs", short(&call));
                let audio = c.audio.clone();
                let cert = c.cert.clone();
                let old_id = c.id;
                if let Some(s) = c.session.take() {
                    let _ = s.send(rtc::Cmd::Stop);
                }
                *c = Call {
                    id: call,
                    peer: from,
                    conversation,
                    outgoing: false,
                    video,
                    phase: Phase::Connecting,
                    connected_at: None,
                    peer_muted: false,
                    audio,
                    cert,
                    session: None,
                    remote: Some(remote),
                    early: Vec::new(),
                    restart_gen: 0,
                };
                drop(current);
                // Tell the platform its outgoing call is now this incoming one
                // under a new id, before any event keyed by the new id.
                emit(CallEvent::Switched {
                    from:  old_id.to_vec(),
                    to:    call.to_vec(),
                    video,
                });
                emit(CallEvent::Connecting { call: call.to_vec() });
                RUNTIME.spawn(spawn_session(call, rtc::Role::Callee, video));
                arm(CONNECT_TIMEOUT, call, Phase::Connecting, |c| {
                    signal(c.conversation, CallMsg::End { call: c.id, reason: CallEnd::Failed });
                    Some(CallEndReason::Failed)
                });
            }
            return;
        },
        Some(_) => {
            info!("CALL[{}]: busy, refusing {}", short(&call), short(&from));
            drop(current);
            signal(conversation, CallMsg::End { call, reason: CallEnd::Busy });
            return;
        },
    }
    let Ok(cert) = new_cert() else { return };
    let Ok(audio) = AudioPath::new() else { return };
    *current = Some(Call {
        id: call,
        peer: from,
        conversation,
        outgoing: false,
        video,
        phase: Phase::Ringing,
        connected_at: None,
        peer_muted: false,
        audio: Arc::new(audio),
        cert,
        session: None,
        remote: Some(remote),
        early: Vec::new(),
        restart_gen: 0,
    });
    drop(current);
    info!("CALL[{}]: ringing, from {}", short(&call), short(&from));
    signal(conversation, CallMsg::Ringing { call });
    emit(CallEvent::Incoming {
        call: call.to_vec(),
        peer: from.to_vec(),
        conversation: conversation.to_vec(),
        video,
    });
    // Stop ringing when the offer dies on the caller's clock, or at the ring
    // timeout, whichever is sooner.
    let left = Duration::from_millis(expires_at_ms.saturating_sub(now_ms()).min(RING_TIMEOUT.as_millis() as u64));
    arm(left, call, Phase::Ringing, |_| Some(CallEndReason::Missed));
}

/// Run `f` on the current call, if any; a `Some` reason ends the call.
fn with_call(f: impl FnOnce(&mut Call) -> Option<CallEndReason>) -> Option<CallEndReason> {
    let mut current = CURRENT.lock();
    let call = current.as_mut()?;
    f(call)
}

/// After `delay`, if the call is still `id` in `phase`, run `f`; a `Some`
/// reason ends the call.
fn arm(
    delay: Duration, id: [u8; 16], phase: Phase,
    f: impl FnOnce(&mut Call) -> Option<CallEndReason> + Send + 'static,
) {
    RUNTIME.spawn(async move {
        tokio::time::sleep(delay).await;
        let ended = with_call(|call| if call.id == id && call.phase == phase { f(call) } else { None });
        if let Some(reason) = ended {
            end(id, reason);
        }
    });
}

fn new_cert() -> Result<DtlsCert> {
    str0m::crypto::from_feature_flags()
        .dtls_provider
        .generate_certificate()
        .ok_or_else(|| anyhow!("no DTLS certificate"))
}

/// Hand the peer's parameters to the session, or hold them until it exists.
fn offer_remote(call: &mut Call, remote: Remote) {
    match &call.session {
        Some(s) => {
            let _ = s.send(rtc::Cmd::Remote {
                ufrag:       remote.ufrag.clone(),
                pwd:         remote.pwd.clone(),
                fingerprint: remote.fingerprint,
                ssrc:        remote.ssrc,
                video_ssrc:  remote.video_ssrc,
                candidates:  remote.candidates.clone(),
            });
            for c in call.early.drain(..) {
                let _ = s.send(rtc::Cmd::Candidate(c));
            }
        },
        None => call.early.extend(remote.candidates.iter().cloned()),
    }
    call.remote = Some(remote);
}

/// Start over on fresh sockets and credentials. `theirs` is the peer's
/// restart when they moved first; it is applied once our session is back.
fn begin_restart(call: &mut Call, theirs: Option<Remote>) {
    if call.phase == Phase::Connected {
        call.phase = Phase::Reconnecting;
        emit(CallEvent::Reconnecting { call: call.id.to_vec() });
        arm(RECONNECT_TIMEOUT, call.id, Phase::Reconnecting, |c| {
            signal(c.conversation, CallMsg::End { call: c.id, reason: CallEnd::Failed });
            Some(CallEndReason::Failed)
        });
    }
    if theirs.is_none() {
        call.restart_gen += 1;
    }
    call.early.clear();
    if let Some(theirs) = theirs {
        // Only the credentials change on a restart; keep their fingerprint.
        call.remote = Some(theirs);
    } else if let Some(r) = call.remote.as_mut() {
        r.candidates.clear();
    }
    if let Some(s) = &call.session {
        let _ = s.send(rtc::Cmd::Restart);
    }
}

/// Fetch TURN credentials and start the media session for call `id`.
async fn spawn_session(id: [u8; 16], role: rtc::Role, video: bool) {
    let relay = match tokio::time::timeout(TURN_TIMEOUT, relay_turn()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!("CALL[{}]: no TURN, direct paths only: {e:#}", short(&id));
            None
        },
        Err(_) => {
            warn!("CALL[{}]: TURN credentials timed out, direct paths only", short(&id));
            None
        },
    };
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let started = with_call(|call| {
        if call.id != id {
            return None;
        }
        let session = rtc::spawn(role, call.cert.clone(), relay, video, call.audio.clone(), ev_tx);
        call.session = Some(session);
        // A callee already holds the caller's parameters; a caller waits for
        // the answer. On a restart both hold what the last round left.
        if let Some(remote) = call.remote.clone() {
            offer_remote(call, remote);
        }
        Some(CallEndReason::Hangup)
    });
    if started.is_none() {
        return;
    }
    while let Some(ev) = ev_rx.recv().await {
        on_session_event(id, ev);
    }
}

fn on_session_event(id: [u8; 16], ev: rtc::Event) {
    let ended = with_call(|call| {
        if call.id != id {
            return None;
        }
        match ev {
            rtc::Event::Local { params, candidates } => {
                let msg = match call.phase {
                    Phase::Offering => CallMsg::Offer {
                        call: id,
                        expires_at_ms: now_ms() + OFFER_LIFE.as_millis() as u64,
                        video: call.video,
                        ufrag: params.ufrag,
                        pwd: params.pwd,
                        fingerprint: params.fingerprint,
                        ssrc: params.ssrc,
                        video_ssrc: params.video_ssrc,
                        candidates,
                    },
                    Phase::Connecting if !call.outgoing => CallMsg::Answer {
                        call: id,
                        ufrag: params.ufrag,
                        pwd: params.pwd,
                        fingerprint: params.fingerprint,
                        ssrc: params.ssrc,
                        video_ssrc: params.video_ssrc,
                        candidates,
                    },
                    Phase::Reconnecting | Phase::Connected => CallMsg::Restart {
                        call: id,
                        generation: call.restart_gen,
                        ufrag: params.ufrag,
                        pwd: params.pwd,
                        candidates,
                    },
                    _ => return None,
                };
                signal(call.conversation, msg);
                None
            },
            rtc::Event::Connected => {
                if matches!(call.phase, Phase::Connecting | Phase::Reconnecting) {
                    info!("CALL[{}]: connected", short(&id));
                    call.phase = Phase::Connected;
                    call.connected_at.get_or_insert_with(Instant::now);
                    emit(CallEvent::Connected { call: id.to_vec() });
                }
                None
            },
            rtc::Event::Disconnected => {
                if call.phase == Phase::Connected {
                    warn!("CALL[{}]: path lost, restarting", short(&id));
                    begin_restart(call, None);
                }
                None
            },
            rtc::Event::Video { frame, keyframe } => {
                if let Some(events) = crate::platform::EVENTS.get() {
                    events.on_call_video(frame, keyframe);
                }
                None
            },
            rtc::Event::KeyframeNeeded => {
                if let Some(events) = crate::platform::EVENTS.get() {
                    events.on_call_video_keyframe();
                }
                None
            },
            rtc::Event::Bitrate(kbps) => {
                if let Some(events) = crate::platform::EVENTS.get() {
                    events.on_call_video_bitrate(kbps);
                }
                None
            },
            rtc::Event::Failed(e) => {
                warn!("CALL[{}]: session failed: {e}", short(&id));
                signal(call.conversation, CallMsg::End { call: id, reason: CallEnd::Failed });
                Some(CallEndReason::Failed)
            },
        }
    });
    if let Some(reason) = ended {
        end(id, reason);
    }
}

/// Tear the call down, record it, and tell the platform. A no-op unless `id`
/// is still the current call: a stale timer or a late signal that validated
/// the old call, released the lock, then raced a replacement must not end the
/// new one under the old reason.
fn end(id: [u8; 16], reason: CallEndReason) {
    let call = {
        let mut current = CURRENT.lock();
        match current.as_ref() {
            Some(c) if c.id == id => current.take().unwrap(),
            _ => return,
        }
    };
    if let Some(s) = &call.session {
        let _ = s.send(rtc::Cmd::Stop);
    }
    let duration_ms = call.connected_at.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
    let outcome = match reason {
        CallEndReason::Hangup | CallEndReason::Failed if call.connected_at.is_some() => {
            format!("answered:{}", duration_ms / 1_000)
        },
        CallEndReason::Hangup | CallEndReason::Cancelled => "cancelled".to_string(),
        CallEndReason::Declined => "declined".to_string(),
        CallEndReason::Busy => "busy".to_string(),
        CallEndReason::Unanswered => "unanswered".to_string(),
        CallEndReason::Missed => "missed".to_string(),
        CallEndReason::Failed => "failed".to_string(),
    };
    info!("CALL[{}]: over, {outcome}", short(&call.id));
    let caller = if call.outgoing { Identity::get().map(|i| i.ipk()).unwrap_or(call.peer) } else { call.peer };
    record(call.conversation, caller, &call.id, &outcome, call.outgoing);
    emit(CallEvent::Ended {
        call: call.id.to_vec(),
        peer: call.peer.to_vec(),
        conversation: call.conversation.to_vec(),
        reason,
        duration_ms,
    });
}

/// The call's row in the chat, keyed by the call id so it lands once.
fn record(conversation: [u8; 16], caller: [u8; 32], id: &[u8; 16], outcome: &str, outgoing: bool) {
    let ts = crate::utils::systime().as_secs();
    if let Err(e) = Message::save_system(conversation, caller, id, SYSTEM_CALL, outcome, ts, outgoing) {
        warn!("CALL[{}]: could not record the call: {e}", short(id));
    }
}

/// Send one signal. Fire-and-forget: an offer that cannot leave ends the
/// call, anything else is best effort.
fn signal(conversation: [u8; 16], msg: CallMsg) {
    let wake = if matches!(msg, CallMsg::Offer { .. }) { Wake::Call } else { Wake::No };
    let is_offer = wake == Wake::Call;
    let id = match &msg {
        CallMsg::Offer { call, .. }
        | CallMsg::Ringing { call }
        | CallMsg::Answer { call, .. }
        | CallMsg::Candidate { call, .. }
        | CallMsg::Media { call, .. }
        | CallMsg::Camera { call, .. }
        | CallMsg::Restart { call, .. }
        | CallMsg::End { call, .. } => *call,
    };
    RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_control_class(conversation, AppPayload::Call(msg), wake).await {
            warn!("CALL[{}]: signal not sent: {e:#}", short(&id));
            if is_offer {
                let ended = with_call(|c| (c.id == id).then_some(CallEndReason::Failed));
                if let Some(reason) = ended {
                    end(id, reason);
                }
            }
        }
    });
}

/// TURN on the relay we are connected to, if it runs one.
async fn relay_turn() -> Result<Option<rtc::Relay>> {
    let (host, port, conn) = {
        let relay = RELAY.read();
        let Some(r) = relay.as_ref() else { return Ok(None) };
        let Some(port) = r.turn_port else { return Ok(None) };
        let Some(conn) = r.connection.clone() else { return Ok(None) };
        (r.host.to_string(), port, conn)
    };
    let (mut tx, mut rx) = conn.open_bi().await?;
    CRelayPacket::TurnCredentials.send(&mut tx).await?;
    let _ = tx.finish();
    match SRelayPacket::unpack(&mut rx).await? {
        SRelayPacket::TurnCredentials(Some(creds)) => Ok(Some(rtc::Relay {
            addr:     std::net::SocketAddr::new(host.parse()?, port),
            username: creds.username,
            password: creds.password,
        })),
        SRelayPacket::TurnCredentials(None) => Ok(None),
        other => bail!("unexpected reply to TurnCredentials: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peer's end reason is read from where we stood: a hangup before we
    /// picked up is a missed call, a hangup before they picked up a cancel.
    #[test]
    fn peer_end_reasons_depend_on_our_phase() {
        let map = |reason, phase| match (reason, phase) {
            (CallEnd::Busy, _) => CallEndReason::Busy,
            (CallEnd::Declined, _) => CallEndReason::Declined,
            (CallEnd::Failed, _) => CallEndReason::Failed,
            (CallEnd::Hangup | CallEnd::Unanswered, Phase::Ringing) => CallEndReason::Missed,
            (CallEnd::Unanswered, _) => CallEndReason::Unanswered,
            (CallEnd::Hangup, Phase::Offering | Phase::Connecting) => CallEndReason::Cancelled,
            (CallEnd::Hangup, _) => CallEndReason::Hangup,
        };
        assert_eq!(map(CallEnd::Hangup, Phase::Ringing), CallEndReason::Missed);
        assert_eq!(map(CallEnd::Hangup, Phase::Connected), CallEndReason::Hangup);
        assert_eq!(map(CallEnd::Hangup, Phase::Offering), CallEndReason::Cancelled);
        assert_eq!(map(CallEnd::Busy, Phase::Offering), CallEndReason::Busy);
    }

    #[test]
    fn nothing_to_do_without_a_call() {
        assert!(current().is_none());
        assert!(accept().is_err());
        reject();
        hangup();
        set_muted(true);
        network_changed();
        let silence = audio_playback(2);
        assert_eq!(silence.len(), 2 * audio::FRAME_SAMPLES * 2);
        assert!(silence.iter().all(|b| *b == 0));
        audio_capture(&[0u8; 4]);
    }
}
