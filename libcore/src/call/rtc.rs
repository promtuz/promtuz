//! The media session: a sans-IO str0m [`Rtc`] on a tokio task, over a direct UDP socket and,
//! when the relay offers one, a TURN allocation.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use common::proto::mls_wire::CallCandidate;
use log::debug;
use log::warn;
use str0m::Candidate;
use str0m::Event as RtcEvent;
use str0m::IceConnectionState;
use str0m::Input;
use str0m::Output;
use str0m::Rtc;
use str0m::RtcConfig;
use str0m::crypto::Fingerprint;
use str0m::crypto::dtls::DtlsCert;
use str0m::format::Codec;
use str0m::media::MediaKind;
use str0m::media::MediaTime;
use str0m::media::Mid;
use str0m::net::Protocol;
use str0m::net::Receive;
use str0m::rtp::Ssrc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use turn::client::Client;
use turn::client::ClientConfig;
use webrtc_util::Conn;

use super::audio::AudioPath;
use super::audio::FRAME_SAMPLES;
use super::audio::SAMPLE_RATE;

use crate::state::core;

/// Fixed media ids, so both ends agree without SDP.
const AUDIO_MID: &str = "0";
const VIDEO_MID: &str = "1";
const SETUP_TIMEOUT: Duration = Duration::from_secs(4);
const VIDEO_START_BITRATE: u32 = 600_000;
const VIDEO_MAX_BITRATE: u32 = 1_000_000;
const VIDEO_MIN_BITRATE: u32 = 120_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Caller,
    Callee,
}

impl Role {
    /// The caller controls ICE and is the DTLS server; the callee is the DTLS client.
    fn controlling(self) -> bool {
        self == Role::Caller
    }

    fn dtls_active(self) -> bool {
        self == Role::Callee
    }
}

/// The relay's TURN server and one client's credentials for it.
pub struct Relay {
    pub addr:     SocketAddr,
    pub username: String,
    pub password: String,
    /// When to move an allocation onto fresh credentials, before the relay stops refreshing it.
    pub renew_at: Instant,
}

pub struct Params {
    pub ufrag:       String,
    pub pwd:         String,
    pub fingerprint: [u8; 32],
    pub ssrc:        u32,
    /// Zero unless this is a video call.
    pub video_ssrc:  u32,
}

pub enum Cmd {
    Audio(Vec<u8>),
    /// An Annex-B H.264 access unit. str0m reads the NAL types, so it needs no keyframe flag.
    Video(Vec<u8>),
    /// The peer's offer, answer, or restart; `video_ssrc` is zero for an audio call.
    Remote {
        ufrag:       String,
        pwd:         String,
        fingerprint: [u8; 32],
        ssrc:        u32,
        video_ssrc:  u32,
        candidates:  Vec<CallCandidate>,
    },
    Candidate(CallCandidate),
    /// Rebinds the sockets after a network change, keeping the ICE and DTLS state.
    Restart,
    Stop,
}

pub enum Event {
    /// Sent once per session and once per restart, with every gathered candidate.
    Local { params: Params, candidates: Vec<CallCandidate> },
    Connected,
    Disconnected,
    /// An Annex-B H.264 access unit from the peer.
    Video { frame: Vec<u8>, keyframe: bool },
    KeyframeNeeded,
    /// Target video bitrate in kbps.
    Bitrate(u32),
    /// The allocation's credentials are due for renewal; a restart moves it onto fresh ones.
    Expiring,
    Failed(String),
}

pub fn spawn(
    role: Role, cert: DtlsCert, relay: Option<Relay>, video: bool, audio: Arc<AudioPath>,
    events: mpsc::UnboundedSender<Event>,
) -> mpsc::UnboundedSender<Cmd> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    core().spawn(async move {
        if let Err(e) = run(role, cert, relay, video, audio, events.clone(), cmd_rx).await {
            let _ = events.send(Event::Failed(format!("{e:#}")));
        }
    });
    cmd_tx
}

struct Transport {
    socket:  Arc<UdpSocket>,
    /// Where str0m sees inbound direct packets arrive, normally the primary host candidate.
    base:    SocketAddr,
    relayed: Option<Relayed>,
    /// Our candidates on these sockets, retired from ICE when a restart replaces them.
    locals:  Vec<Candidate>,
}

struct Relayed {
    conn:  Arc<dyn Conn + Send + Sync>,
    addr:  SocketAddr,
    _turn: TurnClient,
}

/// `listen` leaves a reader task holding the socket until `close`. Dropping this frees the
/// allocation and then the client, whether allocating succeeded, failed, timed out or was
/// replaced.
struct TurnClient {
    client: Client,
    conn:   Option<Arc<dyn Conn + Send + Sync>>,
}

impl Drop for TurnClient {
    fn drop(&mut self) {
        let (client, conn) = (self.client.clone(), self.conn.take());
        // Tracked but not cancellable, so a shutdown still closes the reader.
        let core = core();
        core.tasks.spawn_on(
            async move {
                if let Some(conn) = conn {
                    let _ = conn.close().await;
                }
                let _ = client.close().await;
            },
            &core.runtime,
        );
    }
}

async fn run(
    role: Role, cert: DtlsCert, relay: Option<Relay>, video: bool, audio: Arc<AudioPath>,
    events: mpsc::UnboundedSender<Event>, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) -> Result<()> {
    let mut config = RtcConfig::new()
        .set_dtls_cert(cert.clone())
        // Our own jitter buffer reorders, so str0m releases audio at once.
        .set_reordering_size_audio(0);
    if video {
        config = config.enable_bwe(Some((VIDEO_START_BITRATE as u64).into()));
    }
    let mut rtc = config.build(std::time::Instant::now());
    rtc.direct_api().set_ice_controlling(role.controlling());
    if video {
        rtc.bwe().set_desired_bitrate((VIDEO_MAX_BITRATE as u64).into());
    }

    let mid = Mid::from(AUDIO_MID);
    let our_ssrc = rtc.direct_api().new_ssrc();
    rtc.direct_api().declare_media(mid, MediaKind::Audio);
    rtc.direct_api().declare_stream_tx(our_ssrc, None, mid, None);

    let vmid = Mid::from(VIDEO_MID);
    let our_video_ssrc = video.then(|| {
        let ssrc = rtc.direct_api().new_ssrc();
        rtc.direct_api().declare_media(vmid, MediaKind::Video);
        rtc.direct_api().declare_stream_tx(ssrc, None, vmid, None);
        ssrc
    });

    let opus_pt = codec_pt(&rtc, Codec::Opus).context("no Opus payload type")?;
    let h264_pt = if video { codec_pt(&rtc, Codec::H264) } else { None };

    let creds = rtc.direct_api().local_ice_credentials();
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(&rtc.direct_api().local_dtls_fingerprint().bytes);
    let params = Params {
        ufrag: creds.ufrag,
        pwd: creds.pass,
        fingerprint,
        ssrc: *our_ssrc,
        video_ssrc: our_video_ssrc.map(|s| *s).unwrap_or(0),
    };

    let (transport, candidates) = gather(&relay, &mut rtc).await?;
    events
        .send(Event::Local { params, candidates })
        .map_err(|_| anyhow!("call ended before setup"))?;

    let mut session = Session {
        rtc,
        transport,
        relay,
        audio,
        events,
        mid,
        vmid,
        opus_pt,
        h264_pt,
        role,
        dtls_started: false,
        remote_ssrc: None,
        remote_video_ssrc: None,
        rtp_samples: 0,
        video_start: None,
        connected: false,
        renew_at: None,
    };
    session.renew_at = session.renewal();
    session.event_loop(&mut cmd_rx).await
}

fn codec_pt(rtc: &Rtc, codec: Codec) -> Option<str0m::media::Pt> {
    rtc.codec_config().params().iter().find(|p| p.spec().codec == codec).map(|p| p.pt())
}

async fn gather(relay: &Option<Relay>, rtc: &mut Rtc) -> Result<(Transport, Vec<CallCandidate>)> {
    let socket = Arc::new(UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?);
    let port = socket.local_addr()?.port();

    // The call socket is v4, so only v4 hosts.
    let hosts: Vec<SocketAddr> =
        crate::p2p::candidate::local_candidates(port).into_iter().filter(|a| a.is_ipv4()).collect();
    // A v6-only carrier behind 464XLAT has no v4 host; the call then rides the relay alone.
    let base = hosts.first().copied().unwrap_or(socket.local_addr()?);

    let mut locals = Vec::new();
    let mut wire = Vec::new();
    for host in &hosts {
        locals.extend(Candidate::host(*host, "udp").ok());
        wire.push(CallCandidate::Host { addr: *host });
    }

    // The reflexive base is the primary host, so its transmits leave on the direct socket.
    if let Some(relay) = relay
        && !hosts.is_empty()
    {
        match tokio::time::timeout(SETUP_TIMEOUT, reflexive(&socket, relay.addr)).await {
            Ok(Ok(addr)) if addr != base => {
                locals.extend(Candidate::server_reflexive(addr, base, "udp").ok());
                wire.push(CallCandidate::ServerReflexive { addr, base });
            },
            Ok(Ok(_)) => {},
            Ok(Err(e)) => debug!("CALL: reflexive probe failed: {e}"),
            Err(_) => debug!("CALL: reflexive probe timed out"),
        }
    }

    let mut relayed = None;
    if let Some(relay) = relay {
        match tokio::time::timeout(SETUP_TIMEOUT, allocate(relay)).await {
            Ok(Ok(r)) => {
                locals.extend(Candidate::relayed(r.addr, r.addr, "udp").ok());
                wire.push(CallCandidate::Relayed { addr: r.addr });
                relayed = Some(r);
            },
            Ok(Err(e)) => warn!("CALL: TURN allocation failed: {e:#}"),
            Err(_) => warn!("CALL: TURN allocation timed out"),
        }
    }

    for c in &locals {
        rtc.add_local_candidate(c.clone());
    }
    Ok((Transport { socket, base, relayed, locals }, wire))
}

async fn reflexive(socket: &UdpSocket, stun: SocketAddr) -> Result<SocketAddr> {
    use stun::agent::TransactionId;
    use stun::message::BINDING_REQUEST;
    use stun::message::Getter;
    use stun::message::Message;
    use stun::xoraddr::XorMappedAddress;

    let mut req = Message::new();
    req.build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new())])
        .map_err(|e| anyhow!("build STUN request: {e}"))?;
    socket.send_to(&req.raw, stun).await?;

    let mut buf = [0u8; 512];
    loop {
        let (n, from) = socket.recv_from(&mut buf).await?;
        if from != stun || !stun::message::is_message(&buf[..n]) {
            continue;
        }
        let mut resp = Message::new();
        resp.unmarshal_binary(&buf[..n]).map_err(|e| anyhow!("parse STUN response: {e}"))?;
        let mut mapped =
            XorMappedAddress { ip: IpAddr::from([0, 0, 0, 0]), port: 0 };
        mapped.get_from(&resp).map_err(|e| anyhow!("no mapped address: {e}"))?;
        return Ok(SocketAddr::new(mapped.ip, mapped.port));
    }
}

async fn allocate(relay: &Relay) -> Result<Relayed> {
    let turn_socket = Arc::new(UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?);
    let client = Client::new(ClientConfig {
        stun_serv_addr: relay.addr.to_string(),
        turn_serv_addr: relay.addr.to_string(),
        username:       relay.username.clone(),
        password:       relay.password.clone(),
        realm:          "promtuz".into(),
        software:       String::new(),
        rto_in_ms:      0,
        conn:           turn_socket,
        vnet:           None,
    })
    .await?;
    let mut turn = TurnClient { client, conn: None };
    turn.client.listen().await?;
    let conn: Arc<dyn Conn + Send + Sync> = Arc::new(turn.client.allocate().await?);
    turn.conn = Some(conn.clone());
    let addr = conn.local_addr()?;
    Ok(Relayed { conn, addr, _turn: turn })
}

struct Session {
    rtc:               Rtc,
    transport:         Transport,
    relay:             Option<Relay>,
    audio:             Arc<AudioPath>,
    events:            mpsc::UnboundedSender<Event>,
    mid:               Mid,
    vmid:              Mid,
    opus_pt:           str0m::media::Pt,
    h264_pt:           Option<str0m::media::Pt>,
    role:              Role,
    dtls_started:      bool,
    remote_ssrc:       Option<Ssrc>,
    remote_video_ssrc: Option<Ssrc>,
    rtp_samples:       u64,
    video_start:       Option<Instant>,
    connected:         bool,
    /// Fires [`Event::Expiring`] once, then waits for the next restart to arm it again.
    renew_at:          Option<Instant>,
}

impl Session {
    async fn event_loop(&mut self, cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        let mut relay_buf = vec![0u8; 2048];
        loop {
            let timeout = loop {
                match self.rtc.poll_output()? {
                    Output::Transmit(t) => self.transmit(t).await,
                    Output::Event(e) => {
                        if !self.on_rtc_event(e) {
                            return Ok(());
                        }
                    },
                    Output::Timeout(t) => break t,
                }
                if !self.rtc.is_alive() {
                    return Ok(());
                }
            };

            let wait = timeout.saturating_duration_since(Instant::now());
            let renew = self.renew_at.map(|at| at.saturating_duration_since(Instant::now()));
            let socket = self.transport.socket.clone();
            let base = self.transport.base;
            let relayed = self.transport.relayed.as_ref().map(|r| (r.conn.clone(), r.addr));

            let relay_recv = async {
                match &relayed {
                    Some((conn, _)) => conn.recv_from(&mut relay_buf).await.ok(),
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now()))?;
                },
                _ = tokio::time::sleep(renew.unwrap_or_default()), if renew.is_some() => {
                    self.renew_at = None;
                    let _ = self.events.send(Event::Expiring);
                },
                r = socket.recv_from(&mut buf) => {
                    let (n, from) = r?;
                    self.feed(from, base, &buf[..n]);
                },
                r = relay_recv => {
                    if let (Some((n, from)), Some((_, dest))) = (r, relayed) {
                        self.feed(from, dest, &relay_buf[..n]);
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Stop) | None => return Ok(()),
                    Some(cmd) => self.on_cmd(cmd).await?,
                },
            }
        }
    }

    fn feed(&mut self, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        let Ok(recv) = Receive::new(Protocol::Udp, source, destination, data) else {
            return;
        };
        let input = Input::Receive(Instant::now(), recv);
        if self.rtc.accepts(&input) {
            if let Err(e) = self.rtc.handle_input(input) {
                debug!("CALL: input rejected: {e}");
            }
        }
    }

    async fn transmit(&self, t: str0m::net::Transmit) {
        if let Some(r) = &self.transport.relayed {
            if t.source == r.addr {
                let _ = r.conn.send_to(&t.contents, t.destination).await;
                return;
            }
        }
        let _ = self.transport.socket.send_to(&t.contents, t.destination).await;
    }

    fn on_rtc_event(&mut self, event: RtcEvent) -> bool {
        match event {
            RtcEvent::Connected => {
                self.connected = true;
                let _ = self.events.send(Event::Connected);
            },
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                // Only meaningful once connected; the call machine restarts on it.
                if self.connected {
                    let _ = self.events.send(Event::Disconnected);
                }
            },
            // DTLS connects once. After a restart, ICE coming back up is what says media flows.
            RtcEvent::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) if self.connected => {
                let _ = self.events.send(Event::Connected);
            },
            RtcEvent::MediaData(data) => {
                if data.mid == self.mid {
                    let seq = *(*data.seq_range.end());
                    self.audio.jitter.lock().push(seq, data.data.to_vec());
                } else if data.mid == self.vmid {
                    let keyframe = is_h264_keyframe(&data.data);
                    let _ = self.events.send(Event::Video { frame: data.data.to_vec(), keyframe });
                }
            },
            RtcEvent::KeyframeRequest(req) if req.mid == self.vmid => {
                let _ = self.events.send(Event::KeyframeNeeded);
            },
            RtcEvent::EgressBitrateEstimate(kind) => {
                let (str0m::bwe::BweKind::Twcc(bitrate) | str0m::bwe::BweKind::Remb(_, bitrate)) =
                    kind
                else {
                    return true;
                };
                let kbps = bitrate
                    .as_u64()
                    .clamp(VIDEO_MIN_BITRATE as u64, VIDEO_MAX_BITRATE as u64) as u32
                    / 1000;
                self.rtc.bwe().set_desired_bitrate((VIDEO_MAX_BITRATE as u64).into());
                let _ = self.events.send(Event::Bitrate(kbps));
            },
            _ => {},
        }
        true
    }

    async fn on_cmd(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Audio(packet) => {
                let time =
                    MediaTime::new(self.rtp_samples, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                self.rtp_samples += FRAME_SAMPLES as u64;
                if let Some(writer) = self.rtc.writer(self.mid) {
                    let _ = writer.write(self.opus_pt, Instant::now(), time, packet);
                }
            },
            Cmd::Video(frame) => {
                let Some(pt) = self.h264_pt else { return Ok(()) };
                let now = Instant::now();
                let start = *self.video_start.get_or_insert(now);
                let ticks = (now.duration_since(start).as_millis() as u64) * 90;
                let time = MediaTime::new(ticks, str0m::media::Frequency::NINETY_KHZ);
                if let Some(writer) = self.rtc.writer(self.vmid) {
                    let _ = writer.write(pt, now, time, frame);
                }
            },
            Cmd::Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates } => {
                self.set_remote(ufrag, pwd, fingerprint, ssrc, video_ssrc);
                for c in candidates {
                    self.add_remote(c);
                }
            },
            Cmd::Candidate(c) => self.add_remote(c),
            Cmd::Restart => {
                // The agent keeps its ICE credentials and DTLS keys, so a new pair forms without a
                // handshake. str0m goes unpolled during the bounded gather; ICE tolerates that.
                let creds = self.rtc.direct_api().local_ice_credentials();
                // Near renewal, take fresh credentials now rather than restart for them again soon.
                let soon = Instant::now() + super::TURN_RENEW_EARLY;
                if self.relay.as_ref().is_some_and(|r| r.renew_at <= soon)
                    && let Some(fresh) = super::turn_credentials().await
                {
                    self.relay = Some(fresh);
                }
                // The old sockets go with this restart. Retiring their candidates drops the
                // nominated pair at once, so ICE reports connected again only on a new one.
                for c in &self.transport.locals {
                    self.rtc.direct_api().invalidate_candidate(c);
                }
                let (transport, candidates) = gather(&self.relay, &mut self.rtc).await?;
                // The old allocation closes as its transport drops.
                self.transport = transport;
                self.renew_at = self.renewal();
                let _ = self.events.send(Event::Local {
                    params: Params {
                        ufrag: creds.ufrag,
                        pwd:   creds.pass,
                        // A restart carries only credentials and candidates.
                        fingerprint: [0u8; 32],
                        ssrc: 0,
                        video_ssrc: 0,
                    },
                    candidates,
                });
            },
            Cmd::Stop => {},
        }
        Ok(())
    }

    /// Only an allocation needs renewing, and credentials already due mean renewing just failed.
    fn renewal(&self) -> Option<Instant> {
        let at = self.relay.as_ref()?.renew_at;
        (self.transport.relayed.is_some() && at > Instant::now()).then_some(at)
    }

    fn set_remote(
        &mut self, ufrag: String, pwd: String, fingerprint: [u8; 32], ssrc: u32, video_ssrc: u32,
    ) {
        let mut api = self.rtc.direct_api();
        api.set_remote_ice_credentials(str0m::IceCreds { ufrag, pass: pwd });
        if !self.dtls_started {
            api.set_remote_fingerprint(Fingerprint {
                hash_func: "sha-256".into(),
                bytes:     fingerprint.to_vec(),
            });
            let ssrc = Ssrc::from(ssrc);
            api.expect_stream_rx(ssrc, None, self.mid, None);
            self.remote_ssrc = Some(ssrc);
            if self.h264_pt.is_some() && video_ssrc != 0 {
                let vssrc = Ssrc::from(video_ssrc);
                api.expect_stream_rx(vssrc, None, self.vmid, None);
                self.remote_video_ssrc = Some(vssrc);
            }
            if let Err(e) = api.start_dtls(self.role.dtls_active()) {
                warn!("CALL: DTLS start failed: {e}");
            }
            self.dtls_started = true;
            // Ask for an IDR at once rather than wait out the peer's own timers.
            self.request_peer_keyframe();
        }
    }

    fn request_peer_keyframe(&mut self) {
        if self.remote_video_ssrc.is_none() {
            return;
        }
        if let Some(mut writer) = self.rtc.writer(self.vmid) {
            let _ = writer.request_keyframe(None, str0m::media::KeyframeRequestKind::Pli);
        }
    }

    fn add_remote(&mut self, candidate: CallCandidate) {
        let str0m = match candidate {
            CallCandidate::Host { addr } => Candidate::host(addr, "udp"),
            CallCandidate::ServerReflexive { addr, base } => {
                Candidate::server_reflexive(addr, base, "udp")
            },
            CallCandidate::Relayed { addr } => Candidate::relayed(addr, addr, "udp"),
        };
        if let Ok(c) = str0m {
            self.rtc.add_remote_candidate(c);
        }
    }
}

/// Whether the Annex-B access unit holds an IDR NAL (type 5).
fn is_h264_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        let (start, len) = if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            (i + 3, 3)
        } else if i + 4 <= au.len() && au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            (i + 4, 4)
        } else {
            i += 1;
            continue;
        };
        if start < au.len() && (au[start] & 0x1f) == 5 {
            return true;
        }
        i += len;
    }
    false
}

const _: () = {
    // The engine assumes 20 ms Opus frames at 48 kHz throughout.
    assert!(FRAME_SAMPLES == (SAMPLE_RATE as usize) / 50);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedCore;

    #[tokio::test(start_paused = true)]
    async fn an_allocation_that_times_out_leaves_no_reader_holding_its_socket() {
        let _core = ScopedCore::new();
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = Relay {
            addr:     silent.local_addr().unwrap(),
            username: "u".into(),
            password: "p".into(),
            renew_at: Instant::now(),
        };
        let alive = || tokio::runtime::Handle::current().metrics().num_alive_tasks();
        let before = alive();
        assert!(tokio::time::timeout(SETUP_TIMEOUT, allocate(&relay)).await.is_err());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(alive(), before);
    }

    /// Both ends restart onto new sockets, as after a network change, and media flows again.
    #[tokio::test]
    async fn a_restart_reconnects_and_carries_audio_again() {
        let _core = ScopedCore::new();
        if !crate::p2p::candidate::local_candidates(0).iter().any(|a| a.is_ipv4()) {
            return; // Offline: no interface to make host candidates on.
        }
        let session = |role| {
            let cert = str0m::crypto::from_feature_flags().dtls_provider.generate_certificate();
            let audio = Arc::new(AudioPath::new().unwrap());
            let (tx, rx) = mpsc::unbounded_channel();
            (spawn(role, cert.unwrap(), None, false, audio.clone(), tx), rx, audio)
        };
        let (a, mut a_events, _) = session(Role::Caller);
        let (b, mut b_events, b_audio) = session(Role::Callee);
        let tone = crate::test_support::transfer::tone_packets(25);
        for round in ["setup", "restart"] {
            if round == "restart" {
                a.send(Cmd::Restart).ok().unwrap();
                b.send(Cmd::Restart).ok().unwrap();
            }
            let (pa, ca) = local(&mut a_events).await;
            let (pb, cb) = local(&mut b_events).await;
            a.send(remote(pb, cb)).ok().unwrap();
            b.send(remote(pa, ca)).ok().unwrap();
            connected(&mut a_events).await;
            connected(&mut b_events).await;
            let _ = b_audio.playback(100);
            for packet in &tone {
                a.send(Cmd::Audio(packet.clone())).ok().unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(b_audio.playback(10).iter().any(|s| *s != 0), "after {round}, b hears a");
        }
    }

    async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        match event.expect("an event in time").expect("the session is running") {
            Event::Failed(e) => panic!("session failed: {e}"),
            event => event,
        }
    }

    async fn local(events: &mut mpsc::UnboundedReceiver<Event>) -> (Params, Vec<CallCandidate>) {
        loop {
            if let Event::Local { params, candidates } = next(events).await {
                return (params, candidates);
            }
        }
    }

    async fn connected(events: &mut mpsc::UnboundedReceiver<Event>) {
        while !matches!(next(events).await, Event::Connected) {}
    }

    fn remote(p: Params, candidates: Vec<CallCandidate>) -> Cmd {
        let Params { ufrag, pwd, fingerprint, ssrc, video_ssrc } = p;
        Cmd::Remote { ufrag, pwd, fingerprint, ssrc, video_ssrc, candidates }
    }
}
