//! The attachment/2 frame grammar, used only after mutual identity authentication. Frame bounds
//! are checked before allocation; optional extensions get a separate small budget.

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

    /// Every required capability, ours or the peer's, must be supported by both sides.
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

/// Limits each read to the frames valid at this protocol step, so a server never allocates a
/// manifest where only a small Hello or request is valid.
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

/// Checks the untrusted header before its body is allocated or read. Known kinds cannot be
/// optional, and undefined flag bits are fatal.
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

/// Callers authenticate first and keep their shorter control deadline around this exchange.
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
    use std::fmt::Write as _;

    use expect_test::expect;

    use super::*;

    fn progress() -> Progress {
        Progress::new(Duration::from_secs(1), Duration::from_secs(5))
    }

    fn ranges(spans: &[(u32, u32)]) -> Vec<ChunkRange> {
        spans.iter().map(|&(start, end)| ChunkRange { start, end }).collect()
    }

    #[tokio::test]
    async fn every_frame_round_trips_and_keeps_its_wire_bytes() {
        let manifest =
            Manifest { total_size: 9, chunk_size: 8, chunks: vec![[0x61; 32], [0x62; 32]] };
        let frames = [
            Frame::Hello(Hello::local()),
            Frame::Describe { file_id: [1; 32] },
            Frame::DescribeShared { file_id: [1; 32], grant: [7; 32] },
            Frame::Manifest(manifest.clone()),
            Frame::Manifest(Manifest {
                total_size: 0,
                chunk_size: CHUNK_SIZE as u32,
                chunks:     vec![],
            }),
            Frame::Pull { file_id: [1; 32], ranges: ranges(&[(0, 2), (5, 7)]) },
            Frame::PullShared { file_id: [1; 32], grant: [7; 32], ranges: ranges(&[(3, 4)]) },
            Frame::Chunk { index: 0x12345678, bytes: vec![9, 8, 7] },
            Frame::Complete { chunks_sent: 0x12345678 },
            Frame::Error(ErrorCode::Unavailable),
            Frame::Error(ErrorCode::InvalidRequest),
            Frame::Error(ErrorCode::Unsupported),
            Frame::Error(ErrorCode::Busy),
            Frame::Error(ErrorCode::Storage),
        ];
        let (mut tx, mut rx) = tokio::io::duplex(1024);
        let mut wire = String::new();
        for frame in &frames {
            let mut bytes = Vec::new();
            write_frame_to(&mut bytes, frame, progress()).await.unwrap();
            writeln!(wire, "{}", hex::encode(&bytes)).unwrap();
            write_frame_to(&mut tx, frame, progress()).await.unwrap();
            assert_eq!(
                &read_frame_with_phase_from(&mut rx, progress(), None).await.unwrap(),
                frame
            );
        }
        expect![[r#"
            010001140000000300000000000000010000000000000010004000
            020001200000000101010101010101010101010101010101010101010101010101010101010101
            0800014000000001010101010101010101010101010101010101010101010101010101010101010707070707070707070707070707070707070707070707070707070707070707
            030001500000000900000000000000080000000200000061616161616161616161616161616161616161616161616161616161616161616262626262626262626262626262626262626262626262626262626262626262
            0300011000000000000000000000000000040000000000
            040001320000000101010101010101010101010101010101010101010101010101010101010101020000000000020000000500000007000000
            0900014a0000000101010101010101010101010101010101010101010101010101010101010101070707070707070707070707070707070707070707070707070707070707070701000300000004000000
            0500010700000078563412090807
            0600010400000078563412
            070001020000000100
            070001020000000200
            070001020000000300
            070001020000000400
            070001020000000500
        "#]].assert_eq(&wire);
        for code in [0u16, 6, u16::MAX] {
            assert!(
                decode_body(7, &code.to_le_bytes()).unwrap_err().is::<InvalidFrame>(),
                "{code}"
            );
        }
    }

    /// A body source that fails the test if anything reads it.
    struct Unread;

    impl AsyncRead for Unread {
        fn poll_read(
            self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("the body was read")))
        }
    }

    #[tokio::test]
    async fn hostile_headers_fail_before_their_body_is_read() {
        let mut rows: Vec<(u16, u8, usize, Option<ReadPhase>)> = Vec::new();
        for kind in 1..=9 {
            rows.push((kind, REQUIRED, u32::MAX as usize, None));
            rows.push((kind, 0, 20, None));
        }
        for flags in [2, 3, 128, 255] {
            rows.extend([(1, flags, 20, None), (0xff01, flags, 0, None)]);
        }
        for (kind, exact) in [(1, 20), (2, 32), (6, 4), (7, 2), (8, 64)] {
            rows.extend([(kind, REQUIRED, exact + 1, None), (kind, REQUIRED, exact - 1, None)]);
        }
        rows.extend([
            (0xff01, REQUIRED, 0, None),
            (0xff01, 0, MAX_OPTIONAL_BODY + 1, None),
            (3, REQUIRED, MAX_MANIFEST + 16, None),
            (3, REQUIRED, 17, None),
            (4, REQUIRED, 34, None),
            (4, REQUIRED, 43, None),
            (5, REQUIRED, 3, None),
            (5, REQUIRED, CHUNK_SIZE + 5, None),
        ]);
        for phase in [ReadPhase::Hello, ReadPhase::Request, ReadPhase::Chunk, ReadPhase::Complete] {
            rows.push((3, REQUIRED, 16 + 32 * 100_000, Some(phase)));
        }
        for (kind, flags, len, phase) in rows {
            let mut header = kind.to_le_bytes().to_vec();
            header.push(flags);
            header.extend((len as u32).to_le_bytes());
            let mut source = tokio::io::AsyncReadExt::chain(&header[..], Unread);
            let error =
                read_frame_with_phase_from(&mut source, progress(), phase).await.unwrap_err();
            assert!(error.is::<InvalidFrame>(), "{kind} {flags} {len} {phase:?}: {error}");
        }
    }

    #[test]
    fn capabilities_negotiate_minima_without_downgrading_required_features() {
        let old = Hello { supported: 1, required: 1, ..Hello::local() };
        assert!(!old.negotiate().unwrap().sharing, "an older v2 peer stays range-only");
        assert!(Hello { supported: 3, required: 3, ..Hello::local() }.negotiate().unwrap().sharing);
        let mut peer = Hello::local();
        peer.supported |= 1 << 63;
        peer.max_ranges = 3;
        peer.max_chunks = 9;
        let limits = Limits { sharing: true, max_ranges: 3, max_chunks: 9 };
        assert_eq!(peer.negotiate().unwrap(), limits, "unknown bits are ignored and minima win");
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
            assert!(peer.negotiate().unwrap_err().is::<InvalidFrame>(), "{peer:?}");
        }
    }
}
