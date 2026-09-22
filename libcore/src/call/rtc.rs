//! The media session: one str0m [`Rtc`] driven from a tokio task, sans-IO the
//! same way quinn is. It owns two UDP sockets, a direct one for host and
//! reflexive paths and a TURN allocation for the relayed one, and moves audio
//! between the far end and the [`AudioPath`].
//!
//! Everything the call machine does to a session it does through [`Cmd`];
//! everything the session tells it comes back as [`Event`]. The task lives
//! from [`spawn`] until [`Cmd::Stop`] or a fatal error, and a mid-call network
//! change is a [`Cmd::Restart`] that rebuilds both sockets in place.

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

/// Fixed media ids, so both ends agree without SDP.
const AUDIO_MID: &str = "0";
const VIDEO_MID: &str = "1";
/// str0m's own STUN retransmit gives up a dead pair after a few seconds; a
/// fresh session per network change covers the rest.
const SETUP_TIMEOUT: Duration = Duration::from_secs(4);
/// Video bitrate bounds. The cap is Telegram's; the floor keeps a call alive
/// on a bad link by shedding quality rather than freezing.
const VIDEO_START_BITRATE: u32 = 600_000;
const VIDEO_MAX_BITRATE: u32 = 1_000_000;
const VIDEO_MIN_BITRATE: u32 = 120_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Caller,
    Callee,
}

impl Role {
    /// The caller controls ICE and is the DTLS server; the callee the
    /// reverse, the usual WebRTC actpass resolution.
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
}

/// Our half of the session parameters, for the offer or answer.
pub struct Params {
    pub ufrag:       String,
    pub pwd:         String,
    pub fingerprint: [u8; 32],
    pub ssrc:        u32,
    /// Zero unless this is a video call.
    pub video_ssrc:  u32,
}

pub enum Cmd {
    /// One encoded Opus frame from capture, to send.
    Audio(Vec<u8>),
    /// One encoded H.264 access unit from capture, Annex-B. str0m reads the
    /// NAL types itself, so the sender need not flag a keyframe.
    Video(Vec<u8>),
    /// The peer's session parameters (offer, answer, or restart). `video_ssrc`
    /// is zero for an audio call.
    Remote {
        ufrag:       String,
        pwd:         String,
        fingerprint: [u8; 32],
        ssrc:        u32,
        video_ssrc:  u32,
        candidates:  Vec<CallCandidate>,
    },
    /// One of the peer's trickled candidates.
    Candidate(CallCandidate),
    /// Rebuild on fresh sockets and credentials after a network change.
    Restart,
    Stop,
}

pub enum Event {
    /// Our parameters and candidates, to signal to the peer. Gathering is
    /// synchronous, so a session emits this once (and once per restart) with
    /// every candidate it found; there is no separate trickle.
    Local { params: Params, candidates: Vec<CallCandidate> },
    Connected,
    Disconnected,
    /// An encoded H.264 access unit from the peer, Annex-B, and whether it is
    /// a keyframe.
    Video { frame: Vec<u8>, keyframe: bool },
    /// Our encoder should emit a keyframe (the peer sent a PLI/FIR).
    KeyframeNeeded,
    /// The bandwidth estimate moved; kbps our video encoder should target.
    Bitrate(u32),
    Failed(String),
}

/// Start a session and return the command channel into it. `video` declares a
/// video media section as well as audio; an audio call never carries video.
pub fn spawn(
    role: Role, cert: DtlsCert, relay: Option<Relay>, video: bool, audio: Arc<AudioPath>,
    events: mpsc::UnboundedSender<Event>,
) -> mpsc::UnboundedSender<Cmd> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    crate::RUNTIME.spawn(async move {
        if let Err(e) = run(role, cert, relay, video, audio, events.clone(), cmd_rx).await {
            let _ = events.send(Event::Failed(format!("{e:#}")));
        }
    });
    cmd_tx
}

/// The two sockets a session sends on: the direct one, and the TURN
/// allocation when the relay offered one.
struct Transport {
    socket:  Arc<UdpSocket>,
    /// Our local base address for the direct paths, the primary host
    /// candidate: inbound direct packets are addressed here for str0m.
    base:    SocketAddr,
    relayed: Option<Relayed>,
}

struct Relayed {
    conn: Arc<dyn Conn + Send + Sync>,
    addr: SocketAddr,
}

async fn run(
    role: Role, cert: DtlsCert, relay: Option<Relay>, video: bool, audio: Arc<AudioPath>,
    events: mpsc::UnboundedSender<Event>, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) -> Result<()> {
    let mut config = RtcConfig::new()
        .set_dtls_cert(cert.clone())
        // We run our own jitter buffer, so str0m releases audio immediately.
        .set_reordering_size_audio(0);
    if video {
        // Google's transport-wide congestion control drives the encoder's
        // target bitrate; start it at a conservative estimate.
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
    };
    session.event_loop(&mut cmd_rx).await
}

fn codec_pt(rtc: &Rtc, codec: Codec) -> Option<str0m::media::Pt> {
    rtc.codec_config().params().iter().find(|p| p.spec().codec == codec).map(|p| p.pt())
}

/// Bind the direct socket, allocate the TURN relay, probe the reflexive
/// address, and register every candidate found with str0m.
async fn gather(relay: &Option<Relay>, rtc: &mut Rtc) -> Result<(Transport, Vec<CallCandidate>)> {
    let socket = Arc::new(UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?);
    let port = socket.local_addr()?.port();

    // Host: every routable v4 interface. v6-direct is deferred; the call
    // socket is v4 and a call that can't go direct rides the relay.
    let hosts: Vec<SocketAddr> =
        crate::p2p::candidate::local_candidates(port).into_iter().filter(|a| a.is_ipv4()).collect();
    let base = *hosts.first().context("no routable network interface")?;

    let mut wire = Vec::new();
    for host in &hosts {
        if let Ok(c) = Candidate::host(*host, "udp") {
            rtc.add_local_candidate(c);
        }
        wire.push(CallCandidate::Host { addr: *host });
    }

    // Reflexive: what the direct socket maps to, from the relay's STUN. Its
    // base is the primary host so its transmits egress the direct socket.
    if let Some(relay) = relay {
        match tokio::time::timeout(SETUP_TIMEOUT, reflexive(&socket, relay.addr)).await {
            Ok(Ok(addr)) if addr != base => {
                if let Ok(c) = Candidate::server_reflexive(addr, base, "udp") {
                    rtc.add_local_candidate(c);
                }
                wire.push(CallCandidate::ServerReflexive { addr, base });
            },
            Ok(Ok(_)) => {},
            Ok(Err(e)) => debug!("CALL: reflexive probe failed: {e}"),
            Err(_) => debug!("CALL: reflexive probe timed out"),
        }
    }

    // Relayed: a TURN allocation on the relay. Most phone calls land here.
    let mut relayed = None;
    if let Some(relay) = relay {
        match tokio::time::timeout(SETUP_TIMEOUT, allocate(relay)).await {
            Ok(Ok(r)) => {
                if let Ok(c) = Candidate::relayed(r.addr, r.addr, "udp") {
                    rtc.add_local_candidate(c);
                }
                wire.push(CallCandidate::Relayed { addr: r.addr });
                relayed = Some(r);
            },
            Ok(Err(e)) => warn!("CALL: TURN allocation failed: {e:#}"),
            Err(_) => warn!("CALL: TURN allocation timed out"),
        }
    }

    Ok((Transport { socket, base, relayed }, wire))
}

/// One STUN binding request on `socket` to learn the mapped address.
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

/// Allocate a relayed transport address on the relay's TURN server.
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
    client.listen().await?;
    let conn: Arc<dyn Conn + Send + Sync> = Arc::new(client.allocate().await?);
    let addr = conn.local_addr()?;
    Ok(Relayed { conn, addr })
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
    /// `None` for an audio call.
    h264_pt:           Option<str0m::media::Pt>,
    role:              Role,
    dtls_started:      bool,
    remote_ssrc:       Option<Ssrc>,
    remote_video_ssrc: Option<Ssrc>,
    /// RTP timestamp of the next captured frame, in 48 kHz samples.
    rtp_samples:       u64,
    /// When the first video frame was written, for the 90 kHz RTP clock.
    video_start:       Option<Instant>,
    connected:         bool,
}

impl Session {
    async fn event_loop(&mut self, cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        let mut relay_buf = vec![0u8; 2048];
        loop {
            // Drain str0m's outputs until it asks for a timeout.
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
            // Clone the send/recv handles out so the select borrows them, not
            // `self`, leaving `self` free for the handlers after a branch wins.
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

    /// Feed one received datagram into str0m, addressed to the local base it
    /// arrived at. Non-WebRTC packets fail to parse and are dropped.
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

    /// Send one of str0m's transmits out the socket its source belongs to:
    /// the relayed base goes through TURN, everything else direct.
    async fn transmit(&self, t: str0m::net::Transmit) {
        if let Some(r) = &self.transport.relayed {
            if t.source == r.addr {
                let _ = r.conn.send_to(&t.contents, t.destination).await;
                return;
            }
        }
        let _ = self.transport.socket.send_to(&t.contents, t.destination).await;
    }

    /// Handle a str0m event. Returns false when the session should end.
    fn on_rtc_event(&mut self, event: RtcEvent) -> bool {
        match event {
            RtcEvent::Connected => {
                self.connected = true;
                let _ = self.events.send(Event::Connected);
            },
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                // A pair that stops answering. Only meaningful once we were up;
                // the call machine restarts on it.
                if self.connected {
                    let _ = self.events.send(Event::Disconnected);
                }
            },
            RtcEvent::MediaData(data) => {
                if data.mid == self.mid {
                    let seq = *(*data.seq_range.end());
                    self.audio.jitter.lock().push(seq, data.data.to_vec());
                } else if data.mid == self.vmid {
                    // Encoded H.264, Annex-B, straight to the platform decoder.
                    let keyframe = is_h264_keyframe(&data.data);
                    let _ = self.events.send(Event::Video { frame: data.data.to_vec(), keyframe });
                }
            },
            RtcEvent::KeyframeRequest(req) if req.mid == self.vmid => {
                // The peer wants a fresh IDR from our encoder.
                let _ = self.events.send(Event::KeyframeNeeded);
            },
            RtcEvent::EgressBitrateEstimate(kind) => {
                // BweKind is non-exhaustive; ignore a future variant we can't read.
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
                // 90 kHz RTP clock from the first frame; str0m packetizes the
                // Annex-B access unit into RTP.
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
                // A network change: fresh sockets and candidates on the new
                // interface, trickled into the same agent. str0m keeps its
                // credentials and its DTLS keys, so a new working pair forms
                // without a new handshake. It is unpolled for the bounded
                // gather; the peer knows we are reconnecting and ICE tolerates
                // the gap.
                let creds = self.rtc.direct_api().local_ice_credentials();
                let relay = self.relay.take();
                let (transport, candidates) = gather(&relay, &mut self.rtc).await?;
                self.relay = relay;
                self.transport = transport;
                let _ = self.events.send(Event::Local {
                    params: Params {
                        ufrag: creds.ufrag,
                        pwd:   creds.pass,
                        // Kept from the original handshake; the restart carries
                        // only credentials and candidates.
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
            // Ask the peer for an IDR so our decoder has something to start on
            // rather than waiting out its own retransmit timers.
            self.request_peer_keyframe();
        }
    }

    /// Ask the peer's encoder for a keyframe, if we are receiving video.
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

/// Whether an Annex-B H.264 access unit contains an IDR (keyframe). Scans the
/// NAL headers between start codes for type 5. SPS and PPS precede an IDR in a
/// well-formed keyframe access unit, but the IDR NAL is the definitive marker.
fn is_h264_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        // Match a 3- or 4-byte Annex-B start code.
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
    use super::is_h264_keyframe;

    #[test]
    fn keyframe_detection_reads_nal_types() {
        // NAL type is the low 5 bits of the byte after a start code. 5 = IDR,
        // 1 = non-IDR slice, 7 = SPS, 8 = PPS.
        let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84];
        assert!(is_h264_keyframe(&idr));

        // A real keyframe access unit: SPS, PPS, then the IDR slice.
        let key_au = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, // SPS
            0, 0, 0, 1, 0x68, 0xce, // PPS
            0, 0, 1, 0x65, 0x88, // IDR, 3-byte start code
        ];
        assert!(is_h264_keyframe(&key_au));

        // A delta frame: a non-IDR slice only.
        let delta = [0, 0, 0, 1, 0x41, 0x9a, 0x00];
        assert!(!is_h264_keyframe(&delta));

        // SPS and PPS without an IDR is not, by itself, a decodable keyframe.
        let params_only = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce];
        assert!(!is_h264_keyframe(&params_only));

        assert!(!is_h264_keyframe(&[]));
        assert!(!is_h264_keyframe(&[0, 0, 0, 1]));
    }
}
