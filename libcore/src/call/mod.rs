//! One call per device, driven by a state machine that owns every timer. Signaling rides MLS as
//! [`CallMsg`]; when offers cross, the lower identity key's offer wins and the other side answers.

pub(crate) mod audio;
pub mod ffi;
mod rtc;

use std::collections::VecDeque;
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
use common::utils::now_ms;
use common::utils::now_secs;
use log::debug;
use log::info;
use log::warn;
use parking_lot::Mutex;
use str0m::crypto::dtls::DtlsCert;
use tokio::sync::mpsc;

use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::db::messages::SYSTEM_CALL;
use crate::platform::CallEndReason;
use crate::platform::CallEvent;
use crate::state::core;
use audio::AudioPath;

/// How long the relay holds a call signal before dropping it unread.
pub(crate) const SIGNAL_TTL_MS: u64 = 40_000;
/// The offer's own expiry, on the caller's clock.
const OFFER_LIFE: Duration = Duration::from_secs(40);
const CLOCK_SKEW_MS: u64 = 5_000;
const RING_TIMEOUT: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Asking the relay for TURN credentials must not hold up the ring.
const TURN_TIMEOUT: Duration = Duration::from_secs(3);
/// The relay refuses to refresh an allocation once its credentials expire, so a call moves to
/// fresh ones this long before. It also absorbs clock skew against the relay's expiry.
const TURN_RENEW_EARLY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Offering,
    /// An incoming call rings here.
    Ringing,
    Connecting,
    Connected,
    Reconnecting,
}

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
    cert:         DtlsCert,
    session:      Option<mpsc::UnboundedSender<rtc::Cmd>>,
    remote:       Option<Remote>,
    /// Candidates that arrived before the session did.
    early:        Vec<CallCandidate>,
    /// The latest restart round, ours or theirs.
    restart_gen:  u32,
}

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
/// Effects in the order their transitions held [`CURRENT`], and whether a caller is running them.
static EFFECTS: Mutex<(VecDeque<Effect>, bool)> =
    parking_lot::const_mutex((VecDeque::new(), false));

/// A new call's DTLS certificate and audio path.
type Media = (DtlsCert, Arc<AudioPath>);

/// Everything that moves the call.
enum Input {
    /// Our outgoing call, already built in [`Phase::Offering`].
    Start(Call),
    /// Refused unless this call is the one ringing.
    Accept([u8; 16]),
    Reject([u8; 16]),
    Hangup([u8; 16]),
    Muted(bool),
    Camera(bool),
    NetworkChanged,
    Offer(Offer),
    /// Any signal but an offer.
    Signal(CallMsg),
    /// A timer armed by [`Effect::Arm`] fired.
    Expired([u8; 16], Phase, Expiry),
    /// A media-session event; `now_ms` dates an offer it completes.
    Session { id: [u8; 16], event: rtc::Event, now_ms: u64 },
    /// Sending this call's offer failed.
    OfferLost([u8; 16]),
}

/// An offer and what deciding on it needs from outside the call.
struct Offer {
    from:          [u8; 32],
    conversation:  [u8; 16],
    call:          [u8; 16],
    expires_at_ms: u64,
    video:         bool,
    remote:        Remote,
    /// Only a paired contact may ring us.
    paired:        bool,
    /// Our identity key, for the crossed-offer tie-break.
    me:            [u8; 32],
    now_ms:        u64,
    /// Builds the media for a call that will ring; `None` drops the offer.
    media:         fn() -> Option<Media>,
}

/// What a transition asks for, run in order once `CURRENT` is unlocked.
enum Effect {
    Signal([u8; 16], CallMsg),
    Emit(CallEvent),
    /// Feeds [`Input::Expired`] back after the delay.
    Arm(Duration, [u8; 16], Phase, Expiry),
    StartSession([u8; 16], rtc::Role, bool),
    Record { conversation: [u8; 16], peer: [u8; 32], id: [u8; 16], outcome: String, outgoing: bool },
    Video(Vec<u8>, bool),
    Keyframe,
    Bitrate(u32),
}

/// What a timer does if the call is still where it left it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expiry {
    /// Nobody picked up our call: tell them, and end it.
    Unanswered,
    /// Media did not come up or back in time: tell them, and end it.
    Failed,
    /// We did not pick up theirs.
    Missed,
}

fn rand_id() -> [u8; 16] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b
}

fn emit(event: CallEvent) {
    if let Some(events) = core().events.get() {
        events.on_call(event);
    }
}

fn short(id: &[u8]) -> String {
    hex::encode(&id[..4])
}

/// Applies `input`, then runs what it asked for. Effects queue under the state lock and one caller
/// at a time runs them outside it, so the platform sees events in the order the state changed.
fn apply(input: Input) -> Result<()> {
    {
        let mut current = CURRENT.lock();
        let effects = step(&mut current, input)?;
        let mut queue = EFFECTS.lock();
        queue.0.extend(effects);
        if std::mem::replace(&mut queue.1, true) {
            return Ok(());
        }
    }
    loop {
        let effect = {
            let mut queue = EFFECTS.lock();
            let Some(effect) = queue.0.pop_front() else {
                queue.1 = false;
                return Ok(());
            };
            effect
        };
        // A panic must not leave the queue claimed with nobody running it.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(effect))).is_err() {
            warn!("CALL: an effect panicked");
        }
    }
}

fn run(effect: Effect) {
    match effect {
        Effect::Signal(conversation, msg) => signal(conversation, msg),
        Effect::Emit(event) => emit(event),
        Effect::Arm(delay, id, phase, expiry) => {
            core().spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = apply(Input::Expired(id, phase, expiry));
            });
        },
        Effect::StartSession(id, role, video) => {
            core().spawn(spawn_session(id, role, video));
        },
        Effect::Record { conversation, peer, id, outcome, outgoing } => {
            let caller = if outgoing { Identity::local_ipk().unwrap_or(peer) } else { peer };
            record(conversation, caller, &id, &outcome, outgoing);
        },
        Effect::Video(frame, keyframe) => {
            if let Some(events) = core().events.get() {
                events.on_call_video(frame, keyframe);
            }
        },
        Effect::Keyframe => {
            if let Some(events) = core().events.get() {
                events.on_call_video_keyframe();
            }
        },
        Effect::Bitrate(kbps) => {
            if let Some(events) = core().events.get() {
                events.on_call_video_bitrate(kbps);
            }
        },
    }
}

/// Session commands go straight to the call's session channel; every other effect is returned for
/// [`run`]. An error refuses a user action and changes nothing.
fn step(state: &mut Option<Call>, input: Input) -> Result<Vec<Effect>> {
    let mut fx = Vec::new();
    match input {
        Input::Start(call) => {
            if state.is_some() {
                bail!("already in a call");
            }
            let (id, peer, conversation, video) = (call.id, call.peer, call.conversation, call.video);
            *state = Some(call);
            info!("CALL[{}]: calling {} ({})", short(&id), short(&peer), if video { "video" } else { "audio" });
            fx.push(Effect::Emit(CallEvent::Outgoing {
                call: id.to_vec(),
                peer: peer.to_vec(),
                conversation: conversation.to_vec(),
            }));
            fx.push(Effect::StartSession(id, rtc::Role::Caller, video));
            fx.push(Effect::Arm(RING_TIMEOUT, id, Phase::Offering, Expiry::Unanswered));
        },
        Input::Accept(id) => {
            let ringing = state.as_mut().filter(|c| c.id == id && c.phase == Phase::Ringing);
            let call = ringing.ok_or_else(|| anyhow!("nothing to accept"))?;
            call.phase = Phase::Connecting;
            let video = call.video;
            info!("CALL[{}]: accepted", short(&id));
            fx.push(Effect::Emit(CallEvent::Connecting { call: id.to_vec() }));
            fx.push(Effect::StartSession(id, rtc::Role::Callee, video));
            fx.push(Effect::Arm(CONNECT_TIMEOUT, id, Phase::Connecting, Expiry::Failed));
        },
        Input::Reject(id) => {
            if let Some(call) = state.as_ref().filter(|c| c.id == id && c.phase == Phase::Ringing) {
                fx.push(Effect::Signal(call.conversation, CallMsg::End {
                    call: call.id,
                    reason: CallEnd::Declined,
                }));
                finish(state, CallEndReason::Declined, &mut fx);
            }
        },
        Input::Hangup(id) => {
            if let Some(call) = state.as_ref().filter(|c| c.id == id) {
                let (wire, reason) = match call.phase {
                    Phase::Offering => (CallEnd::Hangup, CallEndReason::Cancelled),
                    Phase::Ringing => (CallEnd::Declined, CallEndReason::Declined),
                    _ => (CallEnd::Hangup, CallEndReason::Hangup),
                };
                fx.push(Effect::Signal(call.conversation, CallMsg::End { call: call.id, reason: wire }));
                finish(state, reason, &mut fx);
            }
        },
        Input::Muted(muted) => {
            if let Some(call) = state.as_mut() {
                call.audio.set_muted(muted);
                if matches!(call.phase, Phase::Connected | Phase::Reconnecting) {
                    fx.push(Effect::Signal(call.conversation, CallMsg::Media { call: call.id, muted }));
                }
            }
        },
        Input::Camera(on) => {
            if let Some(call) = state.as_ref()
                && call.video
                && matches!(call.phase, Phase::Connected | Phase::Reconnecting)
            {
                fx.push(Effect::Signal(call.conversation, CallMsg::Camera { call: call.id, on }));
            }
        },
        Input::NetworkChanged => {
            if let Some(call) = state.as_mut()
                && matches!(call.phase, Phase::Connected | Phase::Reconnecting)
            {
                begin_restart(call, None, &mut fx);
            }
        },
        Input::Offer(offer) => on_offer(state, offer, &mut fx),
        Input::Signal(msg) => on_signal_step(state, msg, &mut fx),
        Input::Expired(id, phase, expiry) => {
            if let Some(call) = state.as_ref().filter(|c| c.id == id && c.phase == phase) {
                let reason = match expiry {
                    Expiry::Unanswered => {
                        info!("CALL[{}]: no answer", short(&call.id));
                        fx.push(Effect::Signal(call.conversation, CallMsg::End {
                            call: call.id,
                            reason: CallEnd::Unanswered,
                        }));
                        CallEndReason::Unanswered
                    },
                    Expiry::Failed => {
                        fx.push(Effect::Signal(call.conversation, CallMsg::End {
                            call: call.id,
                            reason: CallEnd::Failed,
                        }));
                        CallEndReason::Failed
                    },
                    Expiry::Missed => CallEndReason::Missed,
                };
                finish(state, reason, &mut fx);
            }
        },
        Input::Session { id, event, now_ms } => on_session_event(state, id, event, now_ms, &mut fx),
        Input::OfferLost(id) => {
            if state.as_ref().is_some_and(|c| c.id == id) {
                finish(state, CallEndReason::Failed, &mut fx);
            }
        },
    }
    Ok(fx)
}

pub fn start(peer: [u8; 32], video: bool) -> Result<[u8; 16]> {
    if !Contact::is_paired(&peer) {
        bail!("not a paired contact");
    }
    let conversation = Conversation::for_peer(&peer)?;
    let cert = new_cert()?;
    let id = rand_id();
    if CURRENT.lock().is_some() {
        bail!("already in a call");
    }
    let call = Call {
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
    };
    apply(Input::Start(call))?;
    Ok(id)
}

pub fn accept(id: [u8; 16]) -> Result<()> {
    apply(Input::Accept(id))
}

pub fn reject(id: [u8; 16]) {
    let _ = apply(Input::Reject(id));
}

pub fn hangup(id: [u8; 16]) {
    let _ = apply(Input::Hangup(id));
}

pub fn set_muted(muted: bool) {
    let _ = apply(Input::Muted(muted));
}

pub fn network_changed() {
    let _ = apply(Input::NetworkChanged);
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

pub fn set_camera(on: bool) {
    let _ = apply(Input::Camera(on));
}

/// str0m derives keyframes from the NAL types, so the flag goes unused.
pub fn video_capture(frame: Vec<u8>, _keyframe: bool) {
    let session = CURRENT.lock().as_ref().and_then(|c| c.session.clone());
    if let Some(session) = session {
        let _ = session.send(rtc::Cmd::Video(frame));
    }
}

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

pub fn audio_playback(frames: usize) -> Vec<u8> {
    let audio = CURRENT.lock().as_ref().map(|c| c.audio.clone());
    match audio {
        Some(audio) => audio.playback(frames),
        None => vec![0u8; frames * audio::FRAME_SAMPLES * 2],
    }
}

pub(crate) fn on_signal(from: [u8; 32], conversation: [u8; 16], msg: CallMsg) {
    let input = match msg {
        CallMsg::Offer { call, expires_at_ms, video, ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates } => {
            let paired = Contact::is_paired(&from);
            Input::Offer(Offer {
                from,
                conversation,
                call,
                expires_at_ms,
                video,
                remote: Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates },
                paired,
                me: if paired { Identity::local_ipk().unwrap_or([0xff; 32]) } else { [0xff; 32] },
                now_ms: now_ms(),
                media: fresh_media,
            })
        },
        other => Input::Signal(other),
    };
    let _ = apply(input);
}

fn fresh_media() -> Option<Media> {
    let cert = new_cert().ok()?;
    let audio = AudioPath::new().ok()?;
    Some((cert, Arc::new(audio)))
}

fn on_signal_step(state: &mut Option<Call>, msg: CallMsg, fx: &mut Vec<Effect>) {
    match msg {
        // Offers arrive as `Input::Offer`.
        CallMsg::Offer { .. } => {},
        CallMsg::Ringing { call } => {
            if let Some(c) = state.as_ref()
                && c.id == call
                && c.phase == Phase::Offering
            {
                fx.push(Effect::Emit(CallEvent::Ringing { call: call.to_vec() }));
            }
        },
        CallMsg::Answer { call, ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates } => {
            if let Some(c) = state.as_mut()
                && c.id == call
                && c.phase == Phase::Offering
            {
                info!("CALL[{}]: answered", short(&call));
                c.phase = Phase::Connecting;
                let remote = Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates };
                offer_remote(c, remote);
                fx.push(Effect::Emit(CallEvent::Connecting { call: call.to_vec() }));
            }
            fx.push(Effect::Arm(CONNECT_TIMEOUT, call, Phase::Connecting, Expiry::Failed));
        },
        CallMsg::Candidate { call, candidate } => {
            if let Some(c) = state.as_mut()
                && c.id == call
            {
                match &c.session {
                    Some(s) => {
                        let _ = s.send(rtc::Cmd::Candidate(candidate));
                    },
                    None => c.early.push(candidate),
                }
            }
        },
        CallMsg::Media { call, muted } => {
            if let Some(c) = state.as_mut()
                && c.id == call
            {
                c.peer_muted = muted;
                fx.push(Effect::Emit(CallEvent::PeerMuted { call: call.to_vec(), muted }));
            }
        },
        CallMsg::Camera { call, on } => {
            if state.as_ref().is_some_and(|c| c.id == call) {
                fx.push(Effect::Emit(CallEvent::PeerCamera { call: call.to_vec(), on }));
            }
        },
        CallMsg::Restart { call, generation, ufrag, pwd, candidates } => {
            if let Some(c) = state.as_mut()
                && c.id == call
                && matches!(c.phase, Phase::Connected | Phase::Reconnecting)
            {
                let fingerprint = c.remote.as_ref().map(|r| r.fingerprint).unwrap_or_default();
                let ssrc = c.remote.as_ref().map(|r| r.ssrc).unwrap_or_default();
                let video_ssrc = c.remote.as_ref().map(|r| r.video_ssrc).unwrap_or_default();
                let remote = Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates };
                if generation > c.restart_gen {
                    // They restarted first: restart here too, and answer once gathered.
                    c.restart_gen = generation;
                    begin_restart(c, Some(remote), fx);
                } else {
                    // Their answer to our restart, or the same round of a shared drop.
                    offer_remote(c, remote);
                }
            }
        },
        CallMsg::End { call, reason } => {
            let Some(c) = state.as_ref().filter(|c| c.id == call) else { return };
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
            info!("CALL[{}]: ended by peer: {ours:?}", short(&call));
            finish(state, ours, fx);
        },
    }
}

fn on_offer(state: &mut Option<Call>, offer: Offer, fx: &mut Vec<Effect>) {
    let Offer { from, conversation, call, expires_at_ms, video, remote, paired, me, now_ms, media } =
        offer;
    if !paired {
        debug!("CALL[{}]: offer from a stranger ignored", short(&call));
        return;
    }
    if now_ms > expires_at_ms.saturating_add(CLOCK_SKEW_MS) {
        // It stopped ringing before it reached us: a missed call, not a ring.
        info!("CALL[{}]: offer from {} expired in transit", short(&call), short(&from));
        fx.push(Effect::Record {
            conversation,
            peer: from,
            id: call,
            outcome: "missed".into(),
            outgoing: false,
        });
        fx.push(Effect::Emit(CallEvent::Ended {
            call: call.to_vec(),
            peer: from.to_vec(),
            conversation: conversation.to_vec(),
            reason: CallEndReason::Missed,
            duration_ms: 0,
        }));
        return;
    }
    match state.as_mut() {
        None => {},
        Some(c) if c.id == call => return,
        // Crossed offers: the lower key's offer is the call, and the other side answers it.
        Some(c) if c.phase == Phase::Offering && c.peer == from => {
            if from < me {
                info!("CALL[{}]: crossed offers, taking theirs", short(&call));
                let (ours, audio, cert) = (c.id, c.audio.clone(), c.cert.clone());
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
                fx.push(Effect::Emit(CallEvent::Switched {
                    from: ours.to_vec(),
                    to: call.to_vec(),
                    video,
                }));
                fx.push(Effect::Emit(CallEvent::Connecting { call: call.to_vec() }));
                fx.push(Effect::StartSession(call, rtc::Role::Callee, video));
                fx.push(Effect::Arm(CONNECT_TIMEOUT, call, Phase::Connecting, Expiry::Failed));
            }
            return;
        },
        Some(_) => {
            info!("CALL[{}]: busy, refusing {}", short(&call), short(&from));
            fx.push(Effect::Signal(conversation, CallMsg::End { call, reason: CallEnd::Busy }));
            return;
        },
    }
    let Some((cert, audio)) = media() else { return };
    *state = Some(Call {
        id: call,
        peer: from,
        conversation,
        outgoing: false,
        video,
        phase: Phase::Ringing,
        connected_at: None,
        peer_muted: false,
        audio,
        cert,
        session: None,
        remote: Some(remote),
        early: Vec::new(),
        restart_gen: 0,
    });
    info!("CALL[{}]: ringing, from {}", short(&call), short(&from));
    fx.push(Effect::Signal(conversation, CallMsg::Ringing { call }));
    fx.push(Effect::Emit(CallEvent::Incoming {
        call: call.to_vec(),
        peer: from.to_vec(),
        conversation: conversation.to_vec(),
        video,
    }));
    let left = Duration::from_millis(
        expires_at_ms.saturating_sub(now_ms).min(RING_TIMEOUT.as_millis() as u64),
    );
    fx.push(Effect::Arm(left, call, Phase::Ringing, Expiry::Missed));
}

fn new_cert() -> Result<DtlsCert> {
    str0m::crypto::from_feature_flags()
        .dtls_provider
        .generate_certificate()
        .ok_or_else(|| anyhow!("no DTLS certificate"))
}

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

/// `theirs` is the peer's restart when they moved first.
fn begin_restart(call: &mut Call, theirs: Option<Remote>, fx: &mut Vec<Effect>) {
    if call.phase == Phase::Connected {
        call.phase = Phase::Reconnecting;
        fx.push(Effect::Emit(CallEvent::Reconnecting { call: call.id.to_vec() }));
        fx.push(Effect::Arm(RECONNECT_TIMEOUT, call.id, Phase::Reconnecting, Expiry::Failed));
    }
    call.early.clear();
    if let Some(s) = &call.session {
        let _ = s.send(rtc::Cmd::Restart);
    }
    match theirs {
        // Queued behind the restart, so their candidates pair with our new sockets.
        Some(theirs) => offer_remote(call, theirs),
        None => {
            call.restart_gen += 1;
            if let Some(r) = call.remote.as_mut() {
                r.candidates.clear();
            }
        },
    }
}

async fn spawn_session(id: [u8; 16], role: rtc::Role, video: bool) {
    let relay = turn_credentials().await;
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    {
        let mut current = CURRENT.lock();
        let Some(call) = current.as_mut().filter(|c| c.id == id) else { return };
        let session = rtc::spawn(role, call.cert.clone(), relay, video, call.audio.clone(), ev_tx);
        call.session = Some(session);
        // A callee already holds the caller's parameters; a caller waits for the answer.
        if let Some(remote) = call.remote.clone() {
            offer_remote(call, remote);
        }
    }
    while let Some(event) = ev_rx.recv().await {
        let _ = apply(Input::Session { id, event, now_ms: now_ms() });
    }
}

fn on_session_event(
    state: &mut Option<Call>, id: [u8; 16], ev: rtc::Event, now_ms: u64, fx: &mut Vec<Effect>,
) {
    let Some(call) = state.as_mut().filter(|c| c.id == id) else { return };
    match ev {
        rtc::Event::Local { params, candidates } => {
            let msg = match call.phase {
                Phase::Offering => CallMsg::Offer {
                    call: id,
                    expires_at_ms: now_ms + OFFER_LIFE.as_millis() as u64,
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
                _ => return,
            };
            fx.push(Effect::Signal(call.conversation, msg));
        },
        rtc::Event::Connected => {
            if matches!(call.phase, Phase::Connecting | Phase::Reconnecting) {
                info!("CALL[{}]: connected", short(&id));
                call.phase = Phase::Connected;
                call.connected_at.get_or_insert_with(Instant::now);
                fx.push(Effect::Emit(CallEvent::Connected { call: id.to_vec() }));
            }
        },
        rtc::Event::Disconnected => {
            if call.phase == Phase::Connected {
                warn!("CALL[{}]: path lost, restarting", short(&id));
                begin_restart(call, None, fx);
            }
        },
        rtc::Event::Expiring => {
            if matches!(call.phase, Phase::Connected | Phase::Reconnecting) {
                info!("CALL[{}]: TURN credentials expiring, restarting", short(&id));
                begin_restart(call, None, fx);
            }
        },
        rtc::Event::Video { frame, keyframe } => fx.push(Effect::Video(frame, keyframe)),
        rtc::Event::KeyframeNeeded => fx.push(Effect::Keyframe),
        rtc::Event::Bitrate(kbps) => fx.push(Effect::Bitrate(kbps)),
        rtc::Event::Failed(e) => {
            warn!("CALL[{}]: session failed: {e}", short(&id));
            fx.push(Effect::Signal(call.conversation, CallMsg::End { call: id, reason: CallEnd::Failed }));
            finish(state, CallEndReason::Failed, fx);
        },
    }
}

/// Ends the current call: its session stops, and the row and the platform learn how it went.
fn finish(state: &mut Option<Call>, reason: CallEndReason, fx: &mut Vec<Effect>) {
    let Some(call) = state.take() else { return };
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
    fx.push(Effect::Record {
        conversation: call.conversation,
        peer: call.peer,
        id: call.id,
        outcome,
        outgoing: call.outgoing,
    });
    fx.push(Effect::Emit(CallEvent::Ended {
        call: call.id.to_vec(),
        peer: call.peer.to_vec(),
        conversation: call.conversation.to_vec(),
        reason,
        duration_ms,
    }));
}

/// Keyed by the call id, so the row lands once.
fn record(conversation: [u8; 16], caller: [u8; 32], id: &[u8; 16], outcome: &str, outgoing: bool) {
    let ts = now_secs();
    if let Err(e) = Message::save_system(conversation, caller, id, SYSTEM_CALL, outcome, ts, outgoing) {
        warn!("CALL[{}]: could not record the call: {e}", short(id));
    }
}

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
    core().spawn(async move {
        if let Err(e) = crate::messaging::send_control_class(conversation, AppPayload::Call(msg), wake).await {
            warn!("CALL[{}]: signal not sent: {e:#}", short(&id));
            if is_offer {
                let _ = apply(Input::OfferLost(id));
            }
        }
    });
}

/// `None` leaves the call on direct paths.
async fn turn_credentials() -> Option<rtc::Relay> {
    match tokio::time::timeout(TURN_TIMEOUT, relay_turn()).await {
        Ok(Ok(relay)) => relay,
        Ok(Err(e)) => {
            warn!("CALL: no TURN, direct paths only: {e:#}");
            None
        },
        Err(_) => {
            warn!("CALL: TURN credentials timed out, direct paths only");
            None
        },
    }
}

async fn relay_turn() -> Result<Option<rtc::Relay>> {
    let (host, port, conn) = {
        let Some(session) = core().session() else { return Ok(None) };
        let Some(port) = session.turn_port else { return Ok(None) };
        (session.relay.host.to_string(), port, session.conn.clone())
    };
    let (mut tx, mut rx) = conn.open_bi().await?;
    CRelayPacket::TurnCredentials.send(&mut tx).await?;
    let _ = tx.finish();
    match SRelayPacket::unpack(&mut rx).await? {
        SRelayPacket::TurnCredentials(Some(creds)) => {
            let left = Duration::from_millis(creds.expires_at_ms.saturating_sub(now_ms()));
            // The floor keeps a clock skewed past the relay's expiry from renewing in a loop.
            let renew_in = left.saturating_sub(TURN_RENEW_EARLY).max(TURN_RENEW_EARLY);
            Ok(Some(rtc::Relay {
                addr:     std::net::SocketAddr::new(host.parse()?, port),
                username: creds.username,
                password: creds.password,
                renew_at: Instant::now() + renew_in,
            }))
        },
        SRelayPacket::TurnCredentials(None) => Ok(None),
        other => bail!("unexpected reply to TurnCredentials: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: [u8; 32] = [5; 32];
    /// Lower than ours, so their crossed offer wins.
    const LOWER: [u8; 32] = [3; 32];
    const HIGHER: [u8; 32] = [7; 32];
    const NOW: u64 = 1_000_000;

    fn remote(ufrag: &str) -> Remote {
        let (pwd, fingerprint) = ("pwd".into(), [4; 32]);
        Remote {
            ufrag: ufrag.into(),
            pwd,
            fingerprint,
            ssrc: 11,
            video_ssrc: 0,
            candidates: vec![],
        }
    }

    fn call(id: u8, peer: [u8; 32], phase: Phase) -> Call {
        let (cert, audio) = fresh_media().unwrap();
        let answered = matches!(phase, Phase::Connecting | Phase::Connected | Phase::Reconnecting);
        Call {
            id: [id; 16],
            peer,
            conversation: [9; 16],
            outgoing: phase == Phase::Offering,
            video: false,
            phase,
            connected_at: (phase == Phase::Connected).then(Instant::now),
            peer_muted: false,
            audio,
            cert,
            session: None,
            remote: answered.then(|| remote("first")),
            early: Vec::new(),
            restart_gen: 0,
        }
    }

    fn offer(from: [u8; 32], id: u8, expires_at_ms: u64, paired: bool) -> Input {
        Input::Offer(Offer {
            from,
            conversation: [9; 16],
            call: [id; 16],
            expires_at_ms,
            video: false,
            remote: remote("offer"),
            paired,
            me: ME,
            now_ms: NOW,
            media: fresh_media,
        })
    }

    /// Where the call stands after `input`, then every effect it asked for, in order.
    fn check(state: &mut Option<Call>, input: Input) -> String {
        let effects = match step(state, input) {
            Ok(effects) => effects.iter().map(describe).collect::<Vec<_>>().join(", "),
            Err(e) => format!("error: {e}"),
        };
        let at = state.as_ref().map_or("idle".into(), |c| format!("{:?} {}", c.phase, c.id[0]));
        format!("{at}: {effects}")
    }

    fn describe(effect: &Effect) -> String {
        let name = |debug: String| debug.split([' ', '{', '(']).next().unwrap().to_owned();
        match effect {
            Effect::Signal(_, CallMsg::End { reason, .. }) => format!("send End {reason:?}"),
            Effect::Signal(_, msg) => format!("send {}", name(format!("{msg:?}"))),
            Effect::Emit(CallEvent::Ended { reason, .. }) => format!("emit Ended {reason:?}"),
            Effect::Emit(event) => format!("emit {}", name(format!("{event:?}"))),
            Effect::Arm(delay, _, phase, expiry) => {
                format!("arm {}s {phase:?} {expiry:?}", delay.as_secs())
            },
            Effect::StartSession(_, role, _) => {
                format!("start {}", if *role == rtc::Role::Caller { "caller" } else { "callee" })
            },
            Effect::Record { outcome, .. } => format!("record {outcome}"),
            Effect::Video(..) | Effect::Keyframe | Effect::Bitrate(_) => "video".into(),
        }
    }

    fn commands(rx: &mut mpsc::UnboundedReceiver<rtc::Cmd>) -> String {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            out.push(match cmd {
                rtc::Cmd::Restart => "restart".into(),
                rtc::Cmd::Stop => "stop".into(),
                rtc::Cmd::Remote { ufrag, fingerprint, ssrc, .. } => {
                    assert_eq!((fingerprint, ssrc), ([4; 32], 11), "kept from the first answer");
                    format!("remote {ufrag}")
                },
                rtc::Cmd::Candidate(_) => "candidate".into(),
                rtc::Cmd::Audio(_) | rtc::Cmd::Video(_) => "media".into(),
            });
        }
        out.join(", ")
    }

    fn with_session(mut call: Call) -> (Option<Call>, mpsc::UnboundedReceiver<rtc::Cmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        call.session = Some(tx);
        (Some(call), rx)
    }

    #[test]
    fn an_offer_rings_once_refuses_a_second_caller_and_is_answered_once() {
        let mut state = None;
        let ring = NOW + 30_000;
        let stale = NOW - CLOCK_SKEW_MS - 1;
        for (input, expected) in [
            (offer(LOWER, 1, ring, false), "idle: "),
            (offer(LOWER, 1, stale, true), "idle: record missed, emit Ended Missed"),
            (
                offer(LOWER, 1, ring, true),
                "Ringing 1: send Ringing, emit Incoming, arm 30s Ringing Missed",
            ),
            (offer(LOWER, 1, ring, true), "Ringing 1: "),
            (offer(HIGHER, 2, ring, true), "Ringing 1: send End Busy"),
            (Input::Accept([2; 16]), "Ringing 1: error: nothing to accept"),
            (
                Input::Accept([1; 16]),
                "Connecting 1: emit Connecting, start callee, arm 20s Connecting Failed",
            ),
            (Input::Accept([1; 16]), "Connecting 1: error: nothing to accept"),
            (Input::Expired([1; 16], Phase::Ringing, Expiry::Missed), "Connecting 1: "),
        ] {
            assert_eq!(check(&mut state, input), expected);
        }
    }

    #[test]
    fn crossed_offers_go_to_the_lower_identity_on_both_sides() {
        let (mut state, mut session) = with_session(call(1, LOWER, Phase::Offering));
        assert_eq!(
            check(&mut state, offer(LOWER, 2, NOW + 30_000, true)),
            "Connecting 2: emit Switched, emit Connecting, start callee, arm 20s Connecting Failed",
            "theirs wins, and we answer it under its id"
        );
        assert_eq!(commands(&mut session), "stop");
        for stale in [
            Input::Expired([1; 16], Phase::Offering, Expiry::Unanswered),
            Input::OfferLost([1; 16]),
            Input::Signal(CallMsg::End { call: [1; 16], reason: CallEnd::Hangup }),
            Input::Hangup([1; 16]),
            Input::Reject([1; 16]),
        ] {
            assert_eq!(check(&mut state, stale), "Connecting 2: ", "the abandoned id ends nothing");
        }
        let mut state = Some(call(1, HIGHER, Phase::Offering));
        assert_eq!(
            check(&mut state, offer(HIGHER, 2, NOW + 30_000, true)),
            "Offering 1: ",
            "ours wins"
        );
    }

    /// A hang-up taken while another caller is still running the switch's effects reaches the
    /// platform after them.
    #[tokio::test]
    async fn effects_reach_the_platform_in_the_order_the_state_changed() {
        let _core = crate::test_support::ScopedCore::new();
        *CURRENT.lock() = Some(call(1, LOWER, Phase::Offering));
        EFFECTS.lock().1 = true;
        apply(offer(LOWER, 2, NOW + 30_000, true)).unwrap();
        apply(Input::Hangup([2; 16])).unwrap();
        let (queued, _) = std::mem::take(&mut *EFFECTS.lock());
        assert_eq!(
            queued.iter().map(describe).collect::<Vec<_>>().join(", "),
            "emit Switched, emit Connecting, start callee, arm 20s Connecting Failed, \
             send End Hangup, record cancelled, emit Ended Hangup"
        );
    }

    #[test]
    fn timers_and_peer_hangups_end_a_call_by_where_it_stood() {
        let unanswered = Input::Expired([1; 16], Phase::Offering, Expiry::Unanswered);
        let missed = Input::Expired([1; 16], Phase::Ringing, Expiry::Missed);
        let end = |reason| Input::Signal(CallMsg::End { call: [1; 16], reason });
        for (phase, input, expected) in [
            (
                Phase::Offering,
                Input::Expired([2; 16], Phase::Offering, Expiry::Unanswered),
                "Offering 1: ",
            ),
            (
                Phase::Offering,
                unanswered,
                "idle: send End Unanswered, record unanswered, emit Ended Unanswered",
            ),
            (Phase::Ringing, missed, "idle: record missed, emit Ended Missed"),
            (
                Phase::Connected,
                Input::Expired([1; 16], Phase::Connecting, Expiry::Failed),
                "Connected 1: ",
            ),
            (
                Phase::Offering,
                Input::Hangup([1; 16]),
                "idle: send End Hangup, record cancelled, emit Ended Cancelled",
            ),
            (
                Phase::Ringing,
                Input::Hangup([1; 16]),
                "idle: send End Declined, record declined, emit Ended Declined",
            ),
            (
                Phase::Connected,
                Input::Hangup([1; 16]),
                "idle: send End Hangup, record answered:0, emit Ended Hangup",
            ),
            (Phase::Ringing, end(CallEnd::Hangup), "idle: record missed, emit Ended Missed"),
            (Phase::Offering, end(CallEnd::Hangup), "idle: record cancelled, emit Ended Cancelled"),
            (Phase::Offering, end(CallEnd::Busy), "idle: record busy, emit Ended Busy"),
            (Phase::Connected, end(CallEnd::Hangup), "idle: record answered:0, emit Ended Hangup"),
        ] {
            let mut state = Some(call(1, LOWER, phase));
            assert_eq!(check(&mut state, input), expected, "{phase:?}");
        }
    }

    #[test]
    fn a_restart_hands_the_peers_new_parameters_to_the_restarted_session() {
        let (mut state, mut session) = with_session(call(1, LOWER, Phase::Connected));
        let restart = |generation, ufrag: &str| {
            let (ufrag, pwd) = (ufrag.into(), "p".into());
            Input::Signal(CallMsg::Restart {
                call: [1; 16],
                generation,
                ufrag,
                pwd,
                candidates: vec![],
            })
        };
        assert_eq!(
            check(&mut state, restart(1, "theirs")),
            "Reconnecting 1: emit Reconnecting, arm 20s Reconnecting Failed",
            "they moved first"
        );
        assert_eq!(commands(&mut session), "restart, remote theirs");
        assert_eq!(check(&mut state, Input::NetworkChanged), "Reconnecting 1: ");
        assert_eq!(commands(&mut session), "restart", "our own round");
        assert_eq!(check(&mut state, restart(2, "answer")), "Reconnecting 1: ");
        assert_eq!(commands(&mut session), "remote answer", "their answer does not restart again");
        let connected =
            Input::Session { id: [1; 16], event: rtc::Event::Connected, now_ms: NOW };
        assert_eq!(check(&mut state, connected), "Connected 1: emit Connected");
    }
}
