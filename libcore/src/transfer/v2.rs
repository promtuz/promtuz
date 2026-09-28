//! Negotiated attachment frames. This grammar is used only after TLS selects
//! attachment/2 and the existing mutual identity authentication succeeds.
//! Frame bounds are checked before allocation; optional extensions have a
//! separate, small budget and share the enclosing progress deadline.

use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, timeout};

use super::ranges::ChunkRange;
use super::wire::{CHUNK_SIZE, InvalidFrame, Manifest};

const HEADER_LEN: usize = 7;
const REQUIRED: u8 = 1;
const RANGE_CAPABILITY: u64 = 1;
const SHARING_CAPABILITY: u64 = 2;
pub(crate) const MAX_RANGES: u16 = 16;
pub(crate) const MAX_CHUNKS: u16 = 64;
const MAX_MANIFEST: usize = 8 * 1024 * 1024;
const MAX_OPTIONAL_BODY: usize = 1024;
const MAX_OPTIONAL_FRAMES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hello {
    pub supported: u64,
    pub required: u64,
    pub max_ranges: u16,
    pub max_chunks: u16,
}

impl Hello {
    pub(crate) fn local() -> Self {
        Self {
            supported: RANGE_CAPABILITY | SHARING_CAPABILITY,
            required: RANGE_CAPABILITY,
            max_ranges: MAX_RANGES,
            max_chunks: MAX_CHUNKS,
        }
    }

    /// Optional capabilities do not affect this version. A peer must support
    /// our required range grammar, and every capability it requires must be
    /// supported both by that peer and by us.
    pub(crate) fn negotiate(self) -> Result<Limits> {
        let local = Self::local();
        if self.required & !self.supported != 0
            || self.required & !local.supported != 0
            || local.required & !self.supported != 0
        {
            return Err(invalid("incompatible attachment capabilities"));
        }
        if self.max_ranges == 0 || self.max_chunks == 0 {
            return Err(invalid("zero attachment range limit"));
        }
        Ok(Limits {
            sharing: self.supported & SHARING_CAPABILITY != 0,
            max_ranges: self.max_ranges.min(local.max_ranges),
            max_chunks: self.max_chunks.min(local.max_chunks),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub sharing: bool,
    pub max_ranges: u16,
    pub max_chunks: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[repr(u16)]
pub(crate) enum ErrorCode {
    #[error("attachment unavailable")]
    Unavailable = 1,
    #[error("invalid attachment request")]
    InvalidRequest = 2,
    #[error("unsupported attachment request")]
    Unsupported = 3,
    #[error("attachment provider busy")]
    Busy = 4,
    #[error("attachment provider storage failure")]
    Storage = 5,
}

impl TryFrom<u16> for ErrorCode {
    type Error = anyhow::Error;

    fn try_from(value: u16) -> Result<Self> {
        match value {
            1 => Ok(Self::Unavailable),
            2 => Ok(Self::InvalidRequest),
            3 => Ok(Self::Unsupported),
            4 => Ok(Self::Busy),
            5 => Ok(Self::Storage),
            _ => Err(invalid("unknown attachment error code")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Frame {
    Hello(Hello),
    Describe { file_id: [u8; 32] },
    Manifest(Manifest),
    Pull { file_id: [u8; 32], ranges: Vec<ChunkRange> },
    Chunk { index: u32, bytes: Vec<u8> },
    Complete { chunks_sent: u32 },
    Error(ErrorCode),
    DescribeShared { file_id: [u8; 32], grant: [u8; 32] },
    PullShared { file_id: [u8; 32], grant: [u8; 32], ranges: Vec<ChunkRange> },
}

/// Constrain allocations to frames meaningful at the current protocol step.
/// In particular, the serving side never allocates a manifest supplied where
/// only a small Hello or request could be valid.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReadPhase {
    Hello,
    Request,
    Manifest,
    Chunk,
    Complete,
}

impl ReadPhase {
    fn accepts(self, kind: u16) -> bool {
        kind == 7
            || match self {
                Self::Hello => kind == 1,
                Self::Request => matches!(kind, 2 | 4 | 8 | 9),
                Self::Manifest => kind == 3,
                Self::Chunk => kind == 5,
                Self::Complete => kind == 6,
            }
    }
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    InvalidFrame(message.into()).into()
}

/// Check the untrusted header before allocating or consuming its body. Known
/// kinds cannot be made optional, and currently undefined flag bits are fatal.
fn validate_header(kind: u16, flags: u8, len: usize) -> Result<()> {
    if flags & !REQUIRED != 0 {
        return Err(invalid("unknown attachment frame flags"));
    }
    let valid_len = match kind {
        1 => len == 20,
        2 => len == 32,
        3 => (16..=MAX_MANIFEST).contains(&len) && (len - 16).is_multiple_of(32),
        4 => (42..=34 + 8 * MAX_RANGES as usize).contains(&len) && (len - 34).is_multiple_of(8),
        5 => (4..=4 + CHUNK_SIZE).contains(&len),
        6 => len == 4,
        7 => len == 2,
        8 => len == 64,
        9 => (74..=66 + 8 * MAX_RANGES as usize).contains(&len) && (len - 66).is_multiple_of(8),
        _ => {
            if flags == REQUIRED {
                return Err(invalid("unknown required attachment frame"));
            }
            if len > MAX_OPTIONAL_BODY {
                return Err(invalid("optional attachment frame too large"));
            }
            return Ok(());
        },
    };
    if flags != REQUIRED {
        return Err(invalid("known attachment frame must be required"));
    }
    if !valid_len {
        return Err(invalid("invalid attachment frame length"));
    }
    Ok(())
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.chunk_size == 0
        || manifest.chunk_size as usize > CHUNK_SIZE
        || manifest.chunks.len() as u64 != manifest.total_size.div_ceil(manifest.chunk_size as u64)
    {
        return Err(invalid("inconsistent attachment manifest"));
    }
    Ok(())
}

fn check_ranges(ranges: &[ChunkRange], limits: Limits) -> Result<u32> {
    if limits.max_ranges == 0
        || limits.max_ranges > MAX_RANGES
        || limits.max_chunks == 0
        || limits.max_chunks > MAX_CHUNKS
    {
        return Err(invalid("invalid negotiated attachment limits"));
    }
    if ranges.is_empty() || ranges.len() > limits.max_ranges as usize {
        return Err(invalid("invalid attachment range count"));
    }
    let mut previous_end = 0;
    let mut chunks = 0u64;
    for range in ranges {
        if range.start >= range.end || range.start < previous_end {
            return Err(invalid("attachment ranges must be sorted and nonoverlapping"));
        }
        chunks += (range.end - range.start) as u64;
        previous_end = range.end;
    }
    if chunks > limits.max_chunks as u64 {
        return Err(invalid("attachment request exceeds chunk limit"));
    }
    Ok(chunks as u32)
}

/// Call only after authorizing and validating the requested manifest. Returns
/// the count expected in Complete, with no iteration over attacker-chosen spans.
pub(crate) fn validate_ranges(
    ranges: &[ChunkRange], manifest: &Manifest, limits: Limits,
) -> Result<u32> {
    validate_manifest(manifest)?;
    let chunks = check_ranges(ranges, limits)?;
    if ranges.iter().any(|range| range.end as usize > manifest.chunks.len()) {
        return Err(invalid("attachment range exceeds manifest"));
    }
    Ok(chunks)
}

fn max_limits() -> Limits {
    Limits { sharing: true, max_ranges: MAX_RANGES, max_chunks: MAX_CHUNKS }
}

/// Validates sizes before constructing the buffer, including for local writes.
fn encode_body(frame: &Frame) -> Result<(u16, Vec<u8>)> {
    let (kind, len) = match frame {
        Frame::Hello(_) => (1, 20),
        Frame::Describe { .. } => (2, 32),
        Frame::Manifest(manifest) => {
            let len = manifest
                .chunks
                .len()
                .checked_mul(32)
                .and_then(|n| n.checked_add(16))
                .ok_or_else(|| invalid("attachment manifest size overflow"))?;
            validate_manifest(manifest)?;
            (3, len)
        },
        Frame::Pull { ranges, .. } => {
            check_ranges(ranges, max_limits())?;
            (4, 34 + ranges.len() * 8)
        },
        Frame::Chunk { bytes, .. } => (
            5,
            bytes.len().checked_add(4).ok_or_else(|| invalid("attachment chunk size overflow"))?,
        ),
        Frame::Complete { .. } => (6, 4),
        Frame::Error(_) => (7, 2),
        Frame::DescribeShared { .. } => (8, 64),
        Frame::PullShared { ranges, .. } => {
            check_ranges(ranges, max_limits())?;
            (9, 66 + ranges.len() * 8)
        },
    };
    validate_header(kind, REQUIRED, len)?;
    let mut body = Vec::with_capacity(len);
    match frame {
        Frame::Hello(hello) => {
            body.extend_from_slice(&hello.supported.to_le_bytes());
            body.extend_from_slice(&hello.required.to_le_bytes());
            body.extend_from_slice(&hello.max_ranges.to_le_bytes());
            body.extend_from_slice(&hello.max_chunks.to_le_bytes());
        },
        Frame::Describe { file_id } => body.extend_from_slice(file_id),
        Frame::DescribeShared { file_id, grant } => {
            body.extend_from_slice(file_id);
            body.extend_from_slice(grant);
        },
        Frame::Manifest(manifest) => {
            body.extend_from_slice(&manifest.total_size.to_le_bytes());
            body.extend_from_slice(&manifest.chunk_size.to_le_bytes());
            body.extend_from_slice(&(manifest.chunks.len() as u32).to_le_bytes());
            for hash in &manifest.chunks {
                body.extend_from_slice(hash);
            }
        },
        Frame::Pull { file_id, ranges } | Frame::PullShared { file_id, ranges, .. } => {
            body.extend_from_slice(file_id);
            if let Frame::PullShared { grant, .. } = frame { body.extend_from_slice(grant); }
            body.extend_from_slice(&(ranges.len() as u16).to_le_bytes());
            for range in ranges {
                body.extend_from_slice(&range.start.to_le_bytes());
                body.extend_from_slice(&range.end.to_le_bytes());
            }
        },
        Frame::Chunk { index, bytes } => {
            body.extend_from_slice(&index.to_le_bytes());
            body.extend_from_slice(bytes);
        },
        Frame::Complete { chunks_sent } => body.extend_from_slice(&chunks_sent.to_le_bytes()),
        Frame::Error(code) => body.extend_from_slice(&(*code as u16).to_le_bytes()),
    }
    Ok((kind, body))
}

fn decode_body(kind: u16, body: &[u8]) -> Result<Frame> {
    validate_header(kind, REQUIRED, body.len())?;
    // Header validation establishes all fixed fields below. Counts are checked
    // against the actual body before constructing either variable-length list.
    Ok(match kind {
        1 => Frame::Hello(Hello {
            supported: u64::from_le_bytes(body[..8].try_into().unwrap()),
            required: u64::from_le_bytes(body[8..16].try_into().unwrap()),
            max_ranges: u16::from_le_bytes(body[16..18].try_into().unwrap()),
            max_chunks: u16::from_le_bytes(body[18..20].try_into().unwrap()),
        }),
        2 => Frame::Describe { file_id: body.try_into().unwrap() },
        3 => {
            let total_size = u64::from_le_bytes(body[..8].try_into().unwrap());
            let chunk_size = u32::from_le_bytes(body[8..12].try_into().unwrap());
            let count = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
            if count != (body.len() - 16) / 32 {
                return Err(invalid("attachment manifest count disagrees with length"));
            }
            if chunk_size == 0
                || chunk_size as usize > CHUNK_SIZE
                || count as u64 != total_size.div_ceil(chunk_size as u64)
            {
                return Err(invalid("inconsistent attachment manifest"));
            }
            let chunks = body[16..].chunks_exact(32).map(|hash| hash.try_into().unwrap()).collect();
            Frame::Manifest(Manifest { total_size, chunk_size, chunks })
        },
        4 | 9 => {
            let file_id = body[..32].try_into().unwrap();
            let base = if kind == 9 { 64 } else { 32 };
            let count = u16::from_le_bytes(body[base..base+2].try_into().unwrap()) as usize;
            if count != (body.len() - base - 2) / 8 {
                return Err(invalid("attachment range count disagrees with length"));
            }
            let ranges: Vec<_> = body[base+2..]
                .chunks_exact(8)
                .map(|range| ChunkRange {
                    start: u32::from_le_bytes(range[..4].try_into().unwrap()),
                    end: u32::from_le_bytes(range[4..].try_into().unwrap()),
                })
                .collect();
            check_ranges(&ranges, max_limits())?;
            if kind == 9 {
                Frame::PullShared { file_id, grant: body[32..64].try_into().unwrap(), ranges }
            } else {
                Frame::Pull { file_id, ranges }
            }
        },
        5 => Frame::Chunk {
            index: u32::from_le_bytes(body[..4].try_into().unwrap()),
            bytes: body[4..].to_vec(),
        },
        6 => Frame::Complete { chunks_sent: u32::from_le_bytes(body.try_into().unwrap()) },
        7 => Frame::Error(ErrorCode::try_from(u16::from_le_bytes(body.try_into().unwrap()))?),
        8 => Frame::DescribeShared { file_id: body[..32].try_into().unwrap(), grant: body[32..].try_into().unwrap() },
        _ => return Err(invalid("unknown attachment frame")),
    })
}

struct Progress {
    idle: Duration,
    deadline: Instant,
}

impl Progress {
    fn new(idle: Duration, total: Duration) -> Self {
        Self { idle, deadline: Instant::now() + total }
    }

    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("attachment frame progress deadline exceeded");
        }
        Ok(self.idle.min(remaining))
    }

    async fn read_exact<R: AsyncRead + Unpin>(
        &self, r: &mut R, mut bytes: &mut [u8],
    ) -> Result<()> {
        while !bytes.is_empty() {
            let n = timeout(self.remaining()?, r.read(bytes))
                .await
                .map_err(|_| anyhow::anyhow!("attachment frame read stalled"))??;
            if n == 0 {
                return Err(invalid("attachment stream ended before expected frame completed"));
            }
            bytes = &mut bytes[n..];
        }
        Ok(())
    }

    async fn write_all<W: AsyncWrite + Unpin>(&self, w: &mut W, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let n = timeout(self.remaining()?, w.write(bytes))
                .await
                .map_err(|_| anyhow::anyhow!("attachment frame write stalled"))??;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }
}

#[cfg(test)]
async fn read_frame_from<R: AsyncRead + Unpin>(r: &mut R, progress: Progress) -> Result<Frame> {
    read_frame_with_phase_from(r, progress, None).await
}

async fn read_frame_with_phase_from<R: AsyncRead + Unpin>(
    r: &mut R, progress: Progress, phase: Option<ReadPhase>,
) -> Result<Frame> {
    let mut optional = 0;
    loop {
        let mut header = [0u8; HEADER_LEN];
        progress.read_exact(r, &mut header).await?;
        let kind = u16::from_le_bytes(header[..2].try_into().unwrap());
        let flags = header[2];
        let len = u32::from_le_bytes(header[3..].try_into().unwrap()) as usize;
        validate_header(kind, flags, len)?;
        if !(1..=9).contains(&kind) {
            if optional == MAX_OPTIONAL_FRAMES {
                return Err(invalid("too many optional attachment frames"));
            }
            optional += 1;
            let mut ignored = [0u8; MAX_OPTIONAL_BODY];
            progress.read_exact(r, &mut ignored[..len]).await?;
            continue;
        }
        if phase.is_some_and(|phase| !phase.accepts(kind)) {
            return Err(invalid("unexpected attachment frame for current phase"));
        }
        let mut body = vec![0u8; len];
        progress.read_exact(r, &mut body).await?;
        return decode_body(kind, &body);
    }
}

async fn write_frame_to<W: AsyncWrite + Unpin>(
    w: &mut W, frame: &Frame, progress: Progress,
) -> Result<()> {
    let (kind, body) = encode_body(frame)?;
    let mut header = [0u8; HEADER_LEN];
    header[..2].copy_from_slice(&kind.to_le_bytes());
    header[2] = REQUIRED;
    header[3..].copy_from_slice(&(body.len() as u32).to_le_bytes());
    progress.write_all(w, &header).await?;
    progress.write_all(w, &body).await
}

#[cfg(test)]
pub(crate) async fn read_frame(r: &mut quinn::RecvStream) -> Result<Frame> {
    read_frame_from(r, Progress::new(super::CHUNK_TIMEOUT, super::CHUNK_DEADLINE)).await
}

pub(crate) async fn read_frame_for(r: &mut quinn::RecvStream, phase: ReadPhase) -> Result<Frame> {
    read_frame_with_phase_from(
        r,
        Progress::new(super::CHUNK_TIMEOUT, super::CHUNK_DEADLINE),
        Some(phase),
    )
    .await
}

pub(crate) async fn write_frame(w: &mut quinn::SendStream, frame: &Frame) -> Result<()> {
    write_frame_to(w, frame, Progress::new(super::CHUNK_TIMEOUT, super::CHUNK_DEADLINE)).await
}

/// Both peers write their bounded Hello first. Callers retain the existing
/// shorter control deadline around this exchange and must authenticate first.
pub(crate) async fn exchange_hello(
    s: &mut quinn::SendStream, r: &mut quinn::RecvStream,
) -> Result<Limits> {
    write_frame(s, &Frame::Hello(Hello::local())).await?;
    match read_frame_for(r, ReadPhase::Hello).await? {
        Frame::Hello(hello) => hello.negotiate(),
        Frame::Error(code) => Err(code.into()),
        _ => Err(invalid("expected attachment Hello")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress() -> Progress {
        Progress::new(Duration::from_secs(1), Duration::from_secs(5))
    }

    fn header(kind: u16, flags: u8, len: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LEN);
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.push(flags);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes
    }

    fn encoded(frame: &Frame) -> Vec<u8> {
        let (kind, body) = encode_body(frame).unwrap();
        let mut bytes = header(kind, REQUIRED, body.len() as u32);
        bytes.extend_from_slice(&body);
        bytes
    }

    async fn parsed(bytes: &[u8]) -> Result<Frame> {
        read_frame_from(&mut &bytes[..], progress()).await
    }

    fn assert_invalid<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.unwrap_err();
        assert!(error.is::<InvalidFrame>(), "expected terminal protocol error, got {error:?}");
    }

    fn manifest(chunks: usize) -> Manifest {
        Manifest { total_size: chunks as u64, chunk_size: 1, chunks: vec![[0x71; 32]; chunks] }
    }

    #[tokio::test]
    async fn grammar_is_fixed_little_endian_and_preserves_manifest_commitment() {
        let hello = Frame::Hello(Hello::local());
        let bytes = encoded(&hello);
        assert_eq!(&bytes[..HEADER_LEN], &[1, 0, 1, 20, 0, 0, 0]);
        assert_eq!(
            &bytes[HEADER_LEN..],
            &[3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 16, 0, 64, 0]
        );
        assert_eq!(parsed(&bytes).await.unwrap(), hello);
        let manifest = Manifest {
            total_size: CHUNK_SIZE as u64 + 7,
            chunk_size: CHUNK_SIZE as u32,
            chunks: vec![[0x61; 32], [0x62; 32]],
        };
        let frames = [
            Frame::Describe { file_id: manifest.file_id() },
            Frame::DescribeShared { file_id: manifest.file_id(), grant: [7; 32] },
            Frame::PullShared { file_id: manifest.file_id(), grant: [7; 32], ranges: vec![ChunkRange { start: 0, end: 2 }] },
            Frame::Manifest(manifest.clone()),
            Frame::Manifest(Manifest {
                total_size: 0,
                chunk_size: CHUNK_SIZE as u32,
                chunks: vec![],
            }),
            Frame::Pull {
                file_id: manifest.file_id(),
                ranges: vec![ChunkRange { start: 0, end: 2 }],
            },
            Frame::Chunk { index: 0x12345678, bytes: vec![9; CHUNK_SIZE] },
            Frame::Complete { chunks_sent: 0x12345678 },
        ];
        for frame in frames {
            let got = parsed(&encoded(&frame)).await.unwrap();
            assert_eq!(got, frame);
            if let Frame::Manifest(decoded) = got {
                if decoded.total_size != 0 {
                    assert_eq!(decoded.file_id(), manifest.file_id());
                }
            }
        }
        let complete = encoded(&Frame::Complete { chunks_sent: 0x12345678 });
        assert_eq!(&complete[HEADER_LEN..], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[tokio::test]
    async fn hostile_headers_fail_without_consuming_or_allocating_the_body() {
        for kind in 1..=9 {
            assert_invalid(parsed(&header(kind, REQUIRED, u32::MAX)).await);
            assert_invalid(parsed(&header(kind, 0, 20)).await);
        }
        assert_invalid(parsed(&header(0xff01, REQUIRED, 0)).await);
        assert_invalid(parsed(&header(0xff01, 0, (MAX_OPTIONAL_BODY + 1) as u32)).await);
        for flags in [2, 3, 128, 255] {
            assert_invalid(parsed(&header(1, flags, 20)).await);
            assert_invalid(parsed(&header(0xff01, flags, 0)).await);
        }
        // For fixed-width frames, even one trailing byte is not an extension.
        for (kind, exact) in [(1, 20), (2, 32), (6, 4), (7, 2)] {
            assert_invalid(parsed(&header(kind, REQUIRED, exact + 1)).await);
            assert_invalid(parsed(&header(kind, REQUIRED, exact - 1)).await);
        }
        assert_invalid(parsed(&header(3, REQUIRED, (MAX_MANIFEST + 16) as u32)).await);
        assert_invalid(parsed(&header(3, REQUIRED, 17)).await);
        assert_invalid(parsed(&header(4, REQUIRED, 34)).await);
        assert_invalid(parsed(&header(4, REQUIRED, 43)).await);
        assert_invalid(parsed(&header(5, REQUIRED, 3)).await);
        assert_invalid(parsed(&header(5, REQUIRED, (CHUNK_SIZE + 5) as u32)).await);
    }

    #[tokio::test]
    async fn phase_rejects_large_unexpected_manifest_before_reading_its_body() {
        struct HeaderOnly([u8; HEADER_LEN], bool);
        impl AsyncRead for HeaderOnly {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                assert!(!self.1, "phase mismatch must fail before requesting the frame body");
                assert!(buf.remaining() >= HEADER_LEN);
                buf.put_slice(&self.0);
                self.1 = true;
                std::task::Poll::Ready(Ok(()))
            }
        }
        // The length is valid for a Manifest, so global frame limits alone
        // would allocate several MiB and attempt to consume its body.
        let manifest_header: [u8; HEADER_LEN] =
            header(3, REQUIRED, 16 + 32 * 100_000).try_into().unwrap();
        for phase in [ReadPhase::Hello, ReadPhase::Request, ReadPhase::Chunk, ReadPhase::Complete] {
            let mut source = HeaderOnly(manifest_header, false);
            let error =
                read_frame_with_phase_from(&mut source, progress(), Some(phase)).await.unwrap_err();
            assert!(error.is::<InvalidFrame>());
            assert!(error.to_string().contains("current phase"));
        }
    }

    #[tokio::test]
    async fn phase_accepts_its_frames_errors_and_bounded_optional_extensions() {
        let frames = [
            (ReadPhase::Hello, Frame::Hello(Hello::local())),
            (ReadPhase::Request, Frame::Describe { file_id: [1; 32] }),
            (
                ReadPhase::Request,
                Frame::Pull { file_id: [1; 32], ranges: vec![ChunkRange { start: 0, end: 1 }] },
            ),
            (ReadPhase::Request, Frame::DescribeShared { file_id: [1; 32], grant: [7; 32] }),
            (ReadPhase::Request, Frame::PullShared { file_id: [1; 32], grant: [7; 32], ranges: vec![ChunkRange { start: 0, end: 1 }] }),
            (ReadPhase::Manifest, Frame::Manifest(manifest(1))),
            (ReadPhase::Chunk, Frame::Chunk { index: 0, bytes: vec![1] }),
            (ReadPhase::Complete, Frame::Complete { chunks_sent: 1 }),
        ];
        for (phase, expected) in frames {
            let mut bytes = header(0xff01, 0, 1);
            bytes.push(0xab);
            bytes.extend_from_slice(&encoded(&expected));
            assert_eq!(
                read_frame_with_phase_from(&mut &bytes[..], progress(), Some(phase)).await.unwrap(),
                expected
            );
            let bytes = encoded(&Frame::Error(ErrorCode::Unavailable));
            assert_eq!(
                read_frame_with_phase_from(&mut &bytes[..], progress(), Some(phase)).await.unwrap(),
                Frame::Error(ErrorCode::Unavailable)
            );
        }

        let mut bytes = Vec::new();
        for _ in 0..=MAX_OPTIONAL_FRAMES {
            bytes.extend_from_slice(&header(0xff01, 0, 0));
        }
        bytes.extend_from_slice(&encoded(&Frame::Hello(Hello::local())));
        assert_invalid(
            read_frame_with_phase_from(&mut &bytes[..], progress(), Some(ReadPhase::Hello)).await,
        );
    }

    #[tokio::test]
    async fn optional_extensions_have_a_per_known_frame_count_and_byte_budget() {
        let hello = encoded(&Frame::Hello(Hello::local()));
        let mut bytes = Vec::new();
        for _ in 0..MAX_OPTIONAL_FRAMES {
            bytes.extend_from_slice(&header(0x0100, 0, MAX_OPTIONAL_BODY as u32));
            bytes.extend_from_slice(&[0xab; MAX_OPTIONAL_BODY]);
        }
        bytes.extend_from_slice(&hello);
        // Each later known frame gets its own extension budget.
        let mut two_frames = bytes.clone();
        two_frames.extend_from_slice(&bytes);
        let mut input = &two_frames[..];
        assert_eq!(
            read_frame_from(&mut input, progress()).await.unwrap(),
            Frame::Hello(Hello::local())
        );
        assert_eq!(
            read_frame_from(&mut input, progress()).await.unwrap(),
            Frame::Hello(Hello::local())
        );
        assert!(input.is_empty());

        bytes.truncate(bytes.len() - hello.len());
        bytes.extend_from_slice(&header(0x0100, 0, 0));
        bytes.extend_from_slice(&hello);
        assert_invalid(parsed(&bytes).await);
        let zero_length = [header(0x0100, 0, 0), hello].concat();
        assert_eq!(parsed(&zero_length).await.unwrap(), Frame::Hello(Hello::local()));
    }

    #[tokio::test]
    async fn clean_eof_in_any_header_or_body_is_a_terminal_protocol_error() {
        for frame in [
            Frame::Hello(Hello::local()),
            Frame::Manifest(manifest(2)),
            Frame::Pull { file_id: [1; 32], ranges: vec![ChunkRange { start: 0, end: 1 }] },
            Frame::DescribeShared { file_id: [1; 32], grant: [7; 32] },
            Frame::PullShared { file_id: [1; 32], grant: [7; 32], ranges: vec![ChunkRange { start: 0, end: 1 }] },
            Frame::Chunk { index: 0, bytes: vec![4; 16] },
            Frame::Complete { chunks_sent: 0 },
            Frame::Error(ErrorCode::Unavailable),
        ] {
            let bytes = encoded(&frame);
            for truncated in 0..bytes.len() {
                assert_invalid(parsed(&bytes[..truncated]).await);
            }
        }
        // An optional extension is not a substitute for the expected frame.
        assert_invalid(parsed(&header(999, 0, 0)).await);
        let mut optional = header(999, 0, 10);
        optional.extend_from_slice(&[0; 9]);
        assert_invalid(parsed(&optional).await);
    }

    #[test]
    fn manifest_counts_and_sizes_are_checked_before_hash_list_construction() {
        let (_, valid) = encode_body(&Frame::Manifest(manifest(1))).unwrap();
        for count in [0, 2, u32::MAX] {
            let mut body = valid.clone();
            body[12..16].copy_from_slice(&count.to_le_bytes());
            assert_invalid(decode_body(3, &body));
        }
        for chunk_size in [0, CHUNK_SIZE as u32 + 1, u32::MAX] {
            let mut body = valid.clone();
            body[8..12].copy_from_slice(&chunk_size.to_le_bytes());
            assert_invalid(decode_body(3, &body));
        }
        for total_size in [0u64, 2, u64::MAX] {
            let mut body = valid.clone();
            body[..8].copy_from_slice(&total_size.to_le_bytes());
            assert_invalid(decode_body(3, &body));
        }
        let too_many = MAX_MANIFEST / 32;
        assert_invalid(encode_body(&Frame::Manifest(manifest(too_many))));
        assert_invalid(encode_body(&Frame::Chunk { index: 0, bytes: vec![0; CHUNK_SIZE + 1] }));
    }

    #[test]
    fn capabilities_negotiate_minima_without_downgrading_required_features() {
        assert!(!Hello { supported: 1, required: 1, ..Hello::local() }.negotiate().unwrap().sharing,
            "old v2 remains range-only");
        assert!(Hello { supported: 3, required: 3, ..Hello::local() }.negotiate().unwrap().sharing);
        let mut peer = Hello::local();
        peer.supported |= 1 << 63;
        peer.max_ranges = 3;
        peer.max_chunks = 9;
        assert_eq!(peer.negotiate().unwrap(), Limits { sharing: true, max_ranges: 3, max_chunks: 9 });
        peer.required = 0;
        peer.max_ranges = u16::MAX;
        peer.max_chunks = u16::MAX;
        assert_eq!(peer.negotiate().unwrap(), max_limits());
        for peer in [
            Hello { supported: 0, ..Hello::local() },
            Hello { required: 4, ..Hello::local() },
            Hello { supported: 5, required: 5, ..Hello::local() },
            Hello { supported: 2, required: 2, ..Hello::local() },
            Hello { max_ranges: 0, ..Hello::local() },
            Hello { max_chunks: 0, ..Hello::local() },
        ] {
            assert_invalid(peer.negotiate());
        }
    }

    #[test]
    fn ranges_reject_overlap_overflow_unsorted_and_negotiated_limit_violations() {
        let mf = manifest(100);
        let valid = [ChunkRange { start: 1, end: 3 }, ChunkRange { start: 5, end: 8 }];
        assert_eq!(validate_ranges(&valid, &mf, max_limits()).unwrap(), 5);
        let adjacent = [ChunkRange { start: 0, end: 1 }, ChunkRange { start: 1, end: 2 }];
        assert_eq!(validate_ranges(&adjacent, &mf, max_limits()).unwrap(), 2);
        let invalid_ranges = [
            vec![],
            vec![ChunkRange { start: 1, end: 1 }],
            vec![ChunkRange { start: 2, end: 1 }],
            vec![ChunkRange { start: 1, end: 4 }, ChunkRange { start: 3, end: 5 }],
            vec![ChunkRange { start: 4, end: 5 }, ChunkRange { start: 0, end: 1 }],
            vec![ChunkRange { start: 0, end: 65 }],
            vec![ChunkRange { start: 0, end: u32::MAX }],
            vec![
                ChunkRange { start: 0, end: u32::MAX / 2 },
                ChunkRange { start: u32::MAX / 2, end: u32::MAX },
            ],
            (0..17).map(|i| ChunkRange { start: i, end: i + 1 }).collect(),
        ];
        for ranges in invalid_ranges {
            assert_invalid(validate_ranges(&ranges, &mf, max_limits()));
        }
        assert_invalid(validate_ranges(&valid, &mf, Limits { sharing: false, max_ranges: 1, max_chunks: 64 }));
        assert_invalid(validate_ranges(&valid, &mf, Limits { sharing: false, max_ranges: 16, max_chunks: 4 }));
        assert_invalid(validate_ranges(&valid, &mf, Limits { sharing: false, max_ranges: 0, max_chunks: 64 }));
        assert_invalid(validate_ranges(&valid, &mf, Limits { sharing: false, max_ranges: 17, max_chunks: 64 }));
        assert_invalid(validate_ranges(&valid, &mf, Limits { sharing: false, max_ranges: 16, max_chunks: 65 }));
        assert_invalid(validate_ranges(&[ChunkRange { start: 99, end: 101 }], &mf, max_limits()));
        assert_invalid(validate_ranges(
            &[ChunkRange { start: 0, end: 1 }],
            &manifest(0),
            max_limits(),
        ));
        let (_, mut body) =
            encode_body(&Frame::Pull { file_id: [0; 32], ranges: valid.to_vec() }).unwrap();
        body[32..34].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_invalid(decode_body(4, &body));
    }

    #[tokio::test]
    async fn remote_errors_remain_typed_and_unknown_codes_are_protocol_errors() {
        for code in [
            ErrorCode::Unavailable,
            ErrorCode::InvalidRequest,
            ErrorCode::Unsupported,
            ErrorCode::Busy,
            ErrorCode::Storage,
        ] {
            assert_eq!(parsed(&encoded(&Frame::Error(code))).await.unwrap(), Frame::Error(code));
            let error: anyhow::Error = code.into();
            assert_eq!(error.downcast_ref::<ErrorCode>(), Some(&code));
        }
        for code in [0u16, 6, u16::MAX] {
            assert_invalid(decode_body(7, &code.to_le_bytes()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_resets_idle_timeout_but_not_total_deadline() {
        let bytes = encoded(&Frame::Chunk { index: 0, bytes: vec![5; 100] });
        let (mut sender, mut receiver) = tokio::io::duplex(1024);
        let writer = tokio::spawn(async move {
            for part in bytes.chunks(20) {
                sender.write_all(part).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let started = Instant::now();
        let got = read_frame_from(
            &mut receiver,
            Progress::new(Duration::from_millis(10), Duration::from_millis(100)),
        )
        .await
        .unwrap();
        assert!(started.elapsed() > Duration::from_millis(10));
        assert_eq!(got, Frame::Chunk { index: 0, bytes: vec![5; 100] });
        writer.await.unwrap();

        // Every optional extension advances bytes often enough for the idle
        // budget, but cannot keep the same frame read alive past its total.
        let (mut sender, mut receiver) = tokio::io::duplex(1024);
        let writer = tokio::spawn(async move {
            for _ in 0..MAX_OPTIONAL_FRAMES {
                if sender.write_all(&header(999, 0, 0)).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let started = Instant::now();
        let error = read_frame_from(
            &mut receiver,
            Progress::new(Duration::from_millis(10), Duration::from_millis(22)),
        )
        .await
        .unwrap_err();
        assert!(!error.is::<InvalidFrame>(), "deadline is a recoverable transport failure");
        assert_eq!(started.elapsed(), Duration::from_millis(22));
        writer.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_stalls_and_transport_resets_remain_transport_failures() {
        let (_sender, mut receiver) = tokio::io::duplex(64);
        let started = Instant::now();
        let error = read_frame_from(
            &mut receiver,
            Progress::new(Duration::from_millis(7), Duration::from_millis(30)),
        )
        .await
        .unwrap_err();
        assert!(!error.is::<InvalidFrame>());
        assert_eq!(started.elapsed(), Duration::from_millis(7));

        struct Reset;
        impl AsyncRead for Reset {
            fn poll_read(
                self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()))
            }
        }
        let error = read_frame_from(&mut Reset, progress()).await.unwrap_err();
        assert!(!error.is::<InvalidFrame>());
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::ConnectionReset
        );

        // A blocked writer uses the same finite idle budget.
        let (mut sender, _receiver) = tokio::io::duplex(1);
        let error = write_frame_to(
            &mut sender,
            &Frame::Hello(Hello::local()),
            Progress::new(Duration::from_millis(7), Duration::from_millis(30)),
        )
        .await
        .unwrap_err();
        assert!(!error.is::<InvalidFrame>());
    }
}
