//! The audio path between the platform's real-time threads and the call
//! session: Opus both ways and an adaptive jitter buffer on the way out.
//!
//! The platform pushes 20 ms of 48 kHz mono PCM per capture tick and pulls
//! the same per playback tick, both across the FFI as little-endian bytes.
//! Capture is encoded on the calling thread and handed to the session; the
//! session parks decoded-later packets here and playback drains them.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;
use opus::Application;
use opus::Bitrate;
use opus::Channels;
use opus::Decoder;
use opus::Encoder;
use parking_lot::Mutex;

pub const SAMPLE_RATE: u32 = 48_000;
/// One frame: 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
const BITRATE: i32 = 32_000;
/// Loss the encoder plans for with its in-band FEC.
const EXPECTED_LOSS_PERCENT: i32 = 10;
/// Opus never produces more than this for one 20 ms frame at our bitrate.
const MAX_PACKET: usize = 400;

/// Playout delay in frames: never under 40 ms, never over 200 ms.
const MIN_DEPTH: usize = 2;
const MAX_DEPTH: usize = 10;

pub struct AudioPath {
    encoder: Mutex<Encoder>,
    pub jitter: Mutex<Jitter>,
    muted: Mutex<bool>,
}

impl AudioPath {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::Bits(BITRATE))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(EXPECTED_LOSS_PERCENT)?;
        Ok(Self {
            encoder: Mutex::new(encoder),
            jitter: Mutex::new(Jitter::new()?),
            muted: Mutex::new(false),
        })
    }

    pub fn set_muted(&self, muted: bool) {
        *self.muted.lock() = muted;
    }

    pub fn muted(&self) -> bool {
        *self.muted.lock()
    }

    /// Encode one captured frame, or `None` while muted or for a frame of
    /// the wrong size. Muted means nothing is sent: the far end's decoder
    /// conceals a short gap and then plays silence.
    pub fn encode(&self, pcm: &[u8]) -> Option<Vec<u8>> {
        if self.muted() || pcm.len() != FRAME_SAMPLES * 2 {
            return None;
        }
        let samples: Vec<i16> =
            pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut out = vec![0u8; MAX_PACKET];
        let n = self.encoder.lock().encode(&samples, &mut out).ok()?;
        out.truncate(n);
        Some(out)
    }

    /// The next `frames` of playback as little-endian PCM.
    pub fn playback(&self, frames: usize) -> Vec<u8> {
        let mut jitter = self.jitter.lock();
        let mut out = Vec::with_capacity(frames * FRAME_SAMPLES * 2);
        for _ in 0..frames {
            for s in jitter.pop() {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }
}

/// Packets waiting for their playout slot, keyed by RTP sequence number.
///
/// Depth adapts to the network: arrivals that land after their slot deepen
/// it, a buffer that runs deep for a while drains a frame at a time. A
/// missing packet is filled from the next one's FEC when that has arrived,
/// and concealed by the decoder otherwise.
pub struct Jitter {
    decoder: Decoder,
    queue: BTreeMap<u64, Vec<u8>>,
    /// The sequence number playback expects next; `None` before the first packet.
    next: Option<u64>,
    /// Frames of delay before playback starts draining.
    depth: usize,
    /// Since when the queue has held more than `depth + 1` frames.
    deep_since: Option<Instant>,
    /// Frames played since the depth last changed.
    since_change: u32,
    /// Consecutive concealed frames with nothing queued behind them: a true
    /// underflow, as when the peer muted or the stream fell far behind.
    starved: u32,
    pub stats: JitterStats,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    pub concealed: u64,
    pub late: u64,
    pub fec: u64,
}

impl Jitter {
    fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)?,
            queue: BTreeMap::new(),
            next: None,
            depth: MIN_DEPTH,
            deep_since: None,
            since_change: 0,
            starved: 0,
            stats: JitterStats::default(),
        })
    }

    /// A packet off the wire. `seq` is the RTP sequence number, extended.
    pub fn push(&mut self, seq: u64, packet: Vec<u8>) {
        self.stats.received += 1;
        if let Some(next) = self.next {
            if seq < next {
                // Behind the play cursor. After a real drain (the peer muted
                // and playback kept advancing the cursor over silence, or a
                // network delay step set the whole stream back) the cursor has
                // run past where the sender resumed: re-prime on this packet
                // rather than rejecting it and every one after. A mere
                // reordering blip, where the buffer only just emptied, is still
                // treated as late.
                if self.starved >= MIN_DEPTH as u32 {
                    self.next = None;
                    self.starved = 0;
                } else {
                    self.stats.late += 1;
                    if self.depth < MAX_DEPTH && self.since_change > 10 {
                        self.depth += 1;
                        self.since_change = 0;
                    }
                    return;
                }
            } else if seq > next + 50 {
                // A jump of a second or more is a new talkspurt after a long
                // silence or a restart: resync instead of concealing 50 frames.
                self.queue.clear();
                self.next = Some(seq);
            }
        }
        self.queue.insert(seq, packet);
        // Runaway backlog, as after playback stalled: play from the newest.
        while self.queue.len() > MAX_DEPTH + 2 {
            let (&first, _) = self.queue.iter().next().unwrap();
            self.queue.remove(&first);
            self.next = self.queue.keys().next().copied();
        }
    }

    /// The next 20 ms of playback. Silence until the buffer has filled to
    /// depth once; from then on every slot is played, filled or concealed.
    pub fn pop(&mut self) -> [i16; FRAME_SAMPLES] {
        let mut pcm = [0i16; FRAME_SAMPLES];
        let Some(next) = self.next.or_else(|| self.queue.keys().next().copied()) else {
            return pcm;
        };
        if self.next.is_none() {
            if self.queue.len() < self.depth {
                return pcm;
            }
            self.next = Some(next);
        }
        self.stats.played += 1;
        self.since_change += 1;

        // A buffer that stays deeper than it needs to be is latency for
        // nothing: after two steady seconds, drop one frame of it.
        if self.queue.len() > self.depth + 1 {
            let since = *self.deep_since.get_or_insert_with(Instant::now);
            if since.elapsed().as_secs() >= 2 && self.depth > MIN_DEPTH {
                self.depth -= 1;
                self.since_change = 0;
                self.deep_since = None;
            }
        } else {
            self.deep_since = None;
        }

        match self.queue.remove(&next) {
            Some(packet) => {
                self.starved = 0;
                if self.decoder.decode(&packet, &mut pcm, false).is_err() {
                    self.conceal(&mut pcm);
                }
            },
            // Lost. The next packet's FEC carries this one when it has
            // arrived; otherwise the decoder extrapolates.
            None => match self.queue.get(&(next + 1)) {
                Some(following)
                    if self.decoder.decode(following, &mut pcm, true).is_ok() =>
                {
                    self.starved = 0;
                    self.stats.fec += 1;
                },
                _ => {
                    self.conceal(&mut pcm);
                    // Concealment with nothing queued behind it is a true
                    // underflow: count it so a resumed stream re-primes rather
                    // than being rejected as late for good. A gap with later
                    // packets waiting is just a hole, not starvation.
                    if self.queue.is_empty() {
                        self.starved += 1;
                    } else {
                        self.starved = 0;
                    }
                },
            },
        }
        self.next = Some(next + 1);
        pcm
    }

    fn conceal(&mut self, pcm: &mut [i16; FRAME_SAMPLES]) {
        self.stats.concealed += 1;
        if self.decoder.decode(&[], pcm, false).is_err() {
            pcm.fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(phase: &mut f32) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAME_SAMPLES * 2);
        for _ in 0..FRAME_SAMPLES {
            *phase += 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
            let s = (phase.sin() * 8000.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn energy(pcm: &[i16]) -> f64 {
        pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / pcm.len() as f64
    }

    #[test]
    fn frames_round_trip_through_opus_and_the_buffer() {
        // The audio device drives push and pop in lockstep, one of each per
        // 20 ms tick, so the buffer holds only its depth. Prime the depth,
        // then interleave.
        let path = AudioPath::new().unwrap();
        let mut phase = 0.0;
        let packets: Vec<Vec<u8>> = (0..22).map(|_| path.encode(&tone(&mut phase)).unwrap()).collect();
        assert!(packets.iter().all(|p| p.len() < MAX_PACKET));
        let mut out = Vec::new();
        {
            let mut jitter = path.jitter.lock();
            jitter.push(0, packets[0].clone());
            jitter.push(1, packets[1].clone());
            for seq in 2..22u64 {
                jitter.push(seq, packets[seq as usize].clone());
                for s in jitter.pop() {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
        assert_eq!(out.len(), 20 * FRAME_SAMPLES * 2);
        let pcm: Vec<i16> = out.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        // The first frames prime the codec; the tail must carry the tone.
        assert!(energy(&pcm[10 * FRAME_SAMPLES..]) > 1_000_000.0, "audio came out silent");
        let stats = path.jitter.lock().stats;
        assert_eq!(stats.played, 20);
        assert_eq!(stats.concealed, 0);
    }

    #[test]
    fn muted_capture_sends_nothing() {
        let path = AudioPath::new().unwrap();
        path.set_muted(true);
        assert!(path.encode(&vec![0u8; FRAME_SAMPLES * 2]).is_none());
        path.set_muted(false);
        assert!(path.encode(&vec![0u8; FRAME_SAMPLES * 2]).is_some());
        assert!(path.encode(&[0u8; 10]).is_none(), "a short frame is refused");
    }

    #[test]
    fn a_lost_packet_is_filled_from_fec_or_concealed_and_playback_never_stalls() {
        let path = AudioPath::new().unwrap();
        let mut phase = 0.0;
        let packets: Vec<Vec<u8>> = (0..32).map(|_| path.encode(&tone(&mut phase)).unwrap()).collect();
        let mut jitter = path.jitter.lock();
        // Prime the depth, then push and pop in lockstep, dropping two packets.
        // The one-frame lead means the packet after a hole is already buffered,
        // so each hole is covered by its follower's FEC rather than concealed.
        jitter.push(0, packets[0].clone());
        jitter.push(1, packets[1].clone());
        for seq in 2..32u64 {
            if seq != 12 && seq != 20 {
                jitter.push(seq, packets[seq as usize].clone());
            }
            jitter.pop();
        }
        let stats = jitter.stats;
        assert_eq!(stats.played, 30, "every slot plays, present or not");
        assert_eq!(stats.fec + stats.concealed, 2, "each hole filled exactly once");
        assert!(stats.fec >= 1, "FEC from the following packet covers a single loss");
    }

    #[test]
    fn late_arrivals_deepen_the_buffer_and_a_deep_buffer_drains() {
        let path = AudioPath::new().unwrap();
        let mut phase = 0.0;
        let mut jitter = path.jitter.lock();
        // A steady stream, one push per pop, so the buffer stays fed (never
        // starves) and more than ten frames play, the gate for deepening.
        for seq in 0..3u64 {
            jitter.push(seq, path.encode(&tone(&mut phase)).unwrap());
        }
        for seq in 3..15u64 {
            jitter.push(seq, path.encode(&tone(&mut phase)).unwrap());
            jitter.pop();
        }
        // A packet arrives behind the cursor while the buffer is still fed: a
        // genuine reordering, not a resume after a drain, so it deepens.
        let late = jitter.next.unwrap() - 1;
        jitter.push(late, path.encode(&tone(&mut phase)).unwrap());
        assert_eq!(jitter.depth, MIN_DEPTH + 1, "a late packet buys a frame of delay");
        assert_eq!(jitter.stats.late, 1);

        // Now it runs deep: the queue holds far more than the target.
        let next = jitter.next.unwrap();
        for i in 0..(MAX_DEPTH as u64) {
            jitter.push(next + i, path.encode(&tone(&mut phase)).unwrap());
        }
        jitter.deep_since = Some(Instant::now() - std::time::Duration::from_secs(3));
        jitter.pop();
        assert_eq!(jitter.depth, MIN_DEPTH, "two steady seconds shed the extra frame");
    }

    #[test]
    fn playout_recovers_after_a_mute_gap() {
        // The peer mutes: contiguous sequence numbers resume where they left
        // off, but the receiver kept popping silence and marched its cursor far
        // ahead. The resumed packets must play, not be rejected as late.
        let path = AudioPath::new().unwrap();
        let mut phase = 0.0;
        let mut jitter = path.jitter.lock();
        for seq in 0..4u64 {
            jitter.push(seq, path.encode(&tone(&mut phase)).unwrap());
        }
        // Play the four, then two seconds (100 frames) of muted silence.
        for _ in 0..104 {
            jitter.pop();
        }
        assert!(jitter.stats.concealed > 50, "the mute played as concealment");
        assert!(jitter.next.unwrap() > 100, "the cursor advanced through the silence");
        let played_before = jitter.stats.played;

        // Unmute: the sender resumes with the next contiguous number, far below
        // the cursor.
        for i in 0..12u64 {
            jitter.push(4 + i, path.encode(&tone(&mut phase)).unwrap());
        }
        let mut out = Vec::new();
        for _ in 0..12 {
            for s in jitter.pop() {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        assert!(jitter.stats.played > played_before, "playback resumed");
        let pcm: Vec<i16> = out.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert!(energy(&pcm) > 100_000.0, "resumed audio came out silent");
    }

    #[test]
    fn a_far_jump_resyncs_instead_of_concealing_the_gap() {
        let path = AudioPath::new().unwrap();
        let mut phase = 0.0;
        let mut jitter = path.jitter.lock();
        for seq in 0..3u64 {
            jitter.push(seq, path.encode(&tone(&mut phase)).unwrap());
        }
        for _ in 0..3 {
            jitter.pop();
        }
        jitter.push(500, path.encode(&tone(&mut phase)).unwrap());
        jitter.push(501, path.encode(&tone(&mut phase)).unwrap());
        jitter.pop();
        assert_eq!(jitter.next, Some(501));
        assert_eq!(jitter.stats.concealed, 0);
    }
}
