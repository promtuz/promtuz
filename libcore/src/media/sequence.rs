//! Small AVIF sequence container writer. AV1 encoding stays in rav1e and the
//! backwards-compatible primary still image stays in avif-serialize. Track,
//! alpha-reference, timing and edit-list layout follows ISO-BMFF/AVIF and libavif.
use super::avif::{be16, be32, boxes};
use anyhow::{Result, anyhow, ensure};

pub(super) struct Sample {
    pub bytes: Vec<u8>,
    pub sync: bool,
}

pub(super) struct Track {
    pub config: Vec<u8>,
    pub samples: Vec<Sample>,
}

#[derive(Default)]
struct Writer(Vec<u8>);
impl Writer {
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn zero(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn atom(&mut self, kind: &[u8; 4], write: impl FnOnce(&mut Self)) {
        let start = self.0.len();
        self.u32(0);
        self.raw(kind);
        write(self);
        let len = (self.0.len() - start) as u32;
        self.0[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32, write: impl FnOnce(&mut Self)) {
        self.atom(kind, |w| {
            w.u32((u32::from(version) << 24) | flags);
            write(w);
        });
    }
    fn matrix(&mut self) {
        for n in [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000] {
            self.u32(n);
        }
    }
}

pub(super) fn mux(
    width: u32, height: u32, colour: Track, alpha: Option<Track>, delays: &[u32],
    repetitions: Option<u16>,
) -> Result<Vec<u8>> {
    ensure!(
        !delays.is_empty()
            && colour.samples.len() == delays.len()
            && alpha.as_ref().is_none_or(|a| a.samples.len() == delays.len()),
        "AVIF encoder frame counts disagree"
    );
    let mut still = Vec::new();
    avif_serialize::Aviffy::new()
        .set_color_primaries(avif_serialize::constants::ColorPrimaries::Bt709)
        .set_transfer_characteristics(avif_serialize::constants::TransferCharacteristics::Srgb)
        .set_matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt601)
        .set_full_color_range(true)
        .write(
            &mut still,
            &colour.samples[0].bytes,
            alpha.as_ref().map(|a| a.samples[0].bytes.as_slice()),
            width,
            height,
            8,
        )?;
    if delays.len() == 1 {
        return Ok(still);
    }

    // The still writer exposes no custom brands. Only our own newly serialized
    // v0 iloc is relocated here; imported AVIF files are never rewritten.
    let old_ftyp_len = be32(&still, 0)? as usize;
    let mut w = Writer::default();
    w.atom(b"ftyp", |w| {
        w.raw(b"avis");
        w.u32(0);
        w.raw(b"avifavismsf1iso8mif1miaf");
    });
    let delta = w.0.len() - old_ftyp_len;
    w.raw(&still[old_ftyp_len..]);
    let top = boxes(&w.0, 0)?;
    let meta = top
        .iter()
        .find(|b| b.kind == b"meta")
        .ok_or_else(|| anyhow!("Missing encoded AVIF metadata"))?;
    let items = boxes(&meta.data[4..], meta.start + meta.header + 4)?;
    let iloc = items
        .iter()
        .find(|b| b.kind == b"iloc")
        .ok_or_else(|| anyhow!("Missing encoded AVIF locations"))?;
    ensure!(
        iloc.data.len() >= 8 && iloc.data[0] == 0 && iloc.data[4] == 0x44 && iloc.data[5] == 0,
        "Unexpected still AVIF location layout"
    );
    let mut pos = 8;
    let mut fixes = Vec::new();
    let mut colour_offsets = Vec::new();
    let mut alpha_offsets = Vec::new();
    for _ in 0..be16(iloc.data, 6)? {
        let id = be16(iloc.data, pos)?;
        ensure!(be16(iloc.data, pos + 4)? == 1, "Unexpected still AVIF extent count");
        let offset = be32(iloc.data, pos + 6)? + delta as u32;
        fixes.push((iloc.start + iloc.header + pos + 6, offset));
        match id {
            1 => colour_offsets.push(offset),
            2 => alpha_offsets.push(offset),
            _ => {},
        }
        pos += 14;
    }
    for (pos, value) in fixes {
        w.0[pos..pos + 4].copy_from_slice(&value.to_be_bytes());
    }
    ensure!(
        colour_offsets.len() == 1 && (alpha.is_none() || alpha_offsets.len() == 1),
        "Missing primary AVIF frame"
    );

    w.atom(b"mdat", |w| {
        for sample in colour.samples.iter().skip(1) {
            colour_offsets.push(w.0.len() as u32);
            w.raw(&sample.bytes);
        }
        if let Some(alpha) = &alpha {
            for sample in alpha.samples.iter().skip(1) {
                alpha_offsets.push(w.0.len() as u32);
                w.raw(&sample.bytes);
            }
        }
    });
    let duration: u64 = delays.iter().map(|v| u64::from(*v)).sum();
    let movie_duration = repetitions.map_or(u64::MAX, |r| duration * (u64::from(r) + 1));
    w.atom(b"moov", |w| {
        w.full(b"mvhd", 1, 0, |w| {
            w.zero(16);
            w.u32(100);
            w.u64(movie_duration);
            w.u32(0x10000);
            w.u16(0x100);
            w.zero(10);
            w.matrix();
            w.zero(24);
            w.u32(if alpha.is_some() { 3 } else { 2 });
        });
        write_track(
            w,
            1,
            width,
            height,
            &colour,
            &colour_offsets,
            delays,
            duration,
            movie_duration,
            repetitions,
            false,
        );
        if let Some(alpha) = alpha {
            write_track(
                w,
                2,
                width,
                height,
                &alpha,
                &alpha_offsets,
                delays,
                duration,
                movie_duration,
                repetitions,
                true,
            );
        }
    });
    Ok(w.0)
}

#[allow(clippy::too_many_arguments)]
fn write_track(
    w: &mut Writer, id: u32, width: u32, height: u32, track: &Track, offsets: &[u32],
    delays: &[u32], duration: u64, movie_duration: u64, repetitions: Option<u16>, alpha: bool,
) {
    w.atom(b"trak", |w| {
        w.full(b"tkhd", 1, 1, |w| {
            w.zero(16);
            w.u32(id);
            w.u32(0);
            w.u64(movie_duration);
            w.zero(16);
            w.matrix();
            w.u32(width << 16);
            w.u32(height << 16);
        });
        if alpha {
            w.atom(b"tref", |w| w.atom(b"auxl", |w| w.u32(1)));
        }
        w.atom(b"edts", |w| {
            w.full(b"elst", 1, u32::from(repetitions != Some(0)), |w| {
                w.u32(1);
                w.u64(duration);
                w.u64(0);
                w.u16(1);
                w.u16(0);
            })
        });
        w.atom(b"mdia", |w| {
            w.full(b"mdhd", 1, 0, |w| {
                w.zero(16);
                w.u32(100);
                w.u64(duration);
                w.u16(0x55c4);
                w.u16(0);
            });
            w.full(b"hdlr", 0, 0, |w| {
                w.u32(0);
                w.raw(if alpha { b"auxv" } else { b"pict" });
                w.zero(13);
            });
            w.atom(b"minf", |w| {
                w.full(b"vmhd", 0, 1, |w| w.zero(8));
                w.atom(b"dinf", |w| {
                    w.full(b"dref", 0, 0, |w| {
                        w.u32(1);
                        w.full(b"url ", 0, 1, |_| {});
                    })
                });
                w.atom(b"stbl", |w| {
                    w.full(b"stsd", 0, 0, |w| {
                        w.u32(1);
                        w.atom(b"av01", |w| {
                            w.zero(6);
                            w.u16(1);
                            w.zero(16);
                            w.u16(width as u16);
                            w.u16(height as u16);
                            w.u32(0x480000);
                            w.u32(0x480000);
                            w.u32(0);
                            w.u16(1);
                            w.zero(32);
                            w.u16(24);
                            w.u16(0xffff);
                            w.atom(b"av1C", |w| w.raw(&track.config));
                            if !alpha {
                                w.atom(b"colr", |w| {
                                    w.raw(b"nclx");
                                    w.u16(1);
                                    w.u16(13);
                                    w.u16(6);
                                    w.raw(&[0x80]);
                                });
                            }
                            w.full(b"ccst", 0, 0, |w| w.u32(0x7c000000));
                            if alpha {
                                w.full(b"auxi", 0, 0, |w| {
                                    w.raw(b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0")
                                });
                            }
                        });
                    });
                    w.full(b"stts", 0, 0, |w| {
                        w.u32(delays.len() as u32);
                        for delay in delays {
                            w.u32(1);
                            w.u32(*delay);
                        }
                    });
                    w.full(b"stsc", 0, 0, |w| {
                        w.u32(1);
                        w.u32(1);
                        w.u32(1);
                        w.u32(1);
                    });
                    w.full(b"stsz", 0, 0, |w| {
                        w.u32(0);
                        w.u32(track.samples.len() as u32);
                        for sample in &track.samples {
                            w.u32(sample.bytes.len() as u32);
                        }
                    });
                    w.full(b"stco", 0, 0, |w| {
                        w.u32(offsets.len() as u32);
                        for offset in offsets {
                            w.u32(*offset);
                        }
                    });
                    w.full(b"stss", 0, 0, |w| {
                        w.u32(track.samples.iter().filter(|s| s.sync).count() as u32);
                        for (i, sample) in track.samples.iter().enumerate() {
                            if sample.sync {
                                w.u32(i as u32 + 1);
                            }
                        }
                    });
                });
            });
        });
    });
}
