//! Opus encoding for capture, and an adaptive jitter buffer that decodes for playback.

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

/// Packets waiting for their playout slot, keyed by extended RTP sequence number.
pub struct Jitter {
    decoder: Decoder,
    queue: BTreeMap<u64, Vec<u8>>,
    /// The sequence number playback expects next; `None` until playback starts.
    next: Option<u64>,
    highest: u64,
    /// Target playout delay in frames.
    depth: usize,
    deep_since: Option<Instant>,
    since_change: u32,
}

impl Jitter {
    fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)?,
            queue: BTreeMap::new(),
            next: None,
            highest: 0,
            depth: MIN_DEPTH,
            deep_since: None,
            since_change: 0,
        })
    }

    /// `seq` is the extended RTP sequence number.
    pub fn push(&mut self, seq: u64, packet: Vec<u8>) {
        self.highest = self.highest.max(seq);
        if let Some(next) = self.next {
            if seq < next {
                // Its slot has played: a late arrival deepens the buffer, up to the ceiling.
                if self.depth < MAX_DEPTH && self.since_change > 10 {
                    self.depth += 1;
                    self.since_change = 0;
                }
                return;
            }
            // A jump of a second or more, after a long silence or a restart, resyncs
            // instead of concealing 50 frames.
            if seq > next + 50 {
                self.queue.clear();
                self.next = Some(seq);
            }
        }
        self.queue.insert(seq, packet);
        // A runaway backlog, as after a playback stall, drops the oldest packets.
        while self.queue.len() > MAX_DEPTH + 2 {
            let (&first, _) = self.queue.iter().next().unwrap();
            self.queue.remove(&first);
            self.next = self.queue.keys().next().copied();
        }
    }

    /// The next 20 ms of playback; silence until the buffer first fills to `depth`.
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
        // The sender is quiet: hold rather than conceal frames that were never sent.
        if self.queue.is_empty() && next > self.highest {
            return pcm;
        }
        self.since_change += 1;

        // A buffer that stays deep for two seconds sheds one frame of delay.
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
                if self.decoder.decode(&packet, &mut pcm, false).is_err() {
                    self.conceal(&mut pcm);
                }
            },
            // Lost: recover it from the next packet's FEC if that is here, else conceal.
            None => match self.queue.get(&(next + 1)) {
                Some(following) if self.decoder.decode(following, &mut pcm, true).is_ok() => {},
                _ => self.conceal(&mut pcm),
            },
        }
        self.next = Some(next + 1);
        pcm
    }

    fn conceal(&mut self, pcm: &mut [i16; FRAME_SAMPLES]) {
        if self.decoder.decode(&[], pcm, false).is_err() {
            pcm.fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::transfer::tone_packets;

    enum Step {
        Push(u64),
        Play(u64),
        /// Plays this packet's forward error correction for the slot before it.
        Fec(u64),
        Hold,
    }

    fn packet(packets: &[Vec<u8>], seq: u64) -> &[u8] {
        &packets[seq as usize % packets.len()]
    }

    /// Every pop must equal what one decoder fed the expected packets in order produces, so a
    /// dropped, concealed or reordered frame shows up as a mismatch.
    fn check(packets: &[Vec<u8>], steps: &[Step]) {
        let mut jitter = Jitter::new().unwrap();
        let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
        for (i, step) in steps.iter().enumerate() {
            let mut expected = [0i16; FRAME_SAMPLES];
            match *step {
                Step::Push(seq) => {
                    jitter.push(seq, packet(packets, seq).to_vec());
                    continue;
                },
                Step::Play(seq) => {
                    reference.decode(packet(packets, seq), &mut expected, false).map(drop)
                },
                Step::Fec(seq) => {
                    reference.decode(packet(packets, seq), &mut expected, true).map(drop)
                },
                Step::Hold => Ok(()),
            }
            .unwrap();
            assert!(jitter.pop() == expected, "step {i}");
        }
    }

    #[test]
    fn the_jitter_buffer_plays_every_frame_it_has_in_order_and_holds_through_pauses() {
        use Step::*;
        let packets = tone_packets(8);
        let after_three = |packet: &[u8], fec: bool| {
            let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
            let mut pcm = [0i16; FRAME_SAMPLES];
            for p in &packets[..3] {
                decoder.decode(p, &mut pcm, false).unwrap();
            }
            decoder.decode(packet, &mut pcm, fec).unwrap();
            pcm
        };
        assert!(after_three(&packets[4], true) != after_three(&[], false), "the packets carry FEC");

        let mut paused = vec![Push(0), Push(1), Push(2), Play(0), Play(1), Play(2)];
        paused.extend((0..50).map(|_| Hold));
        paused.extend([Push(3), Push(4), Play(3), Play(4)]);
        check(&packets, &paused);
        check(&packets, &[Push(0), Push(1), Play(0), Push(3), Play(1), Push(2), Play(2), Play(3)]);
        let lost = [
            Push(0),
            Push(1),
            Play(0),
            Push(2),
            Play(1),
            Push(4),
            Play(2),
            Push(5),
            Fec(4),
            Play(4),
            Play(5),
        ];
        check(&packets, &lost);
        let jump = [
            Push(0),
            Push(1),
            Push(2),
            Play(0),
            Play(1),
            Play(2),
            Push(500),
            Push(501),
            Play(500),
            Play(501),
        ];
        check(&packets, &jump);
    }
}
