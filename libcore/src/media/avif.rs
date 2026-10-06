//! Bounded ISO-BMFF inspection. This does not decode AV1; playback still needs a
//! decoder with its own frame/allocation limits. Unknown properties stay intact.
use anyhow::{Result, anyhow, bail, ensure};
use std::collections::BTreeMap;

use super::validate_dimensions;

pub(super) const MAX_FRAMES: u32 = 600;
pub(super) const MAX_DURATION_CS: u64 = 30_000;
const MAX_BOXES: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct AvifInfo {
    pub width: u32,
    pub height: u32,
    /// Conservative pixels needed for one decoded image, including auxiliary
    /// images and grid assembly, before cropping. Does not multiply by frames
    /// or the AV1 decoder's reference buffers; callers budget those separately.
    pub decoded_pixel_count: u64,
    /// Largest declared coded or grid-canvas edge, including auxiliary images.
    pub max_coded_edge: u32,
    pub animated: bool,
    pub frame_count: u32,
    /// One sequence pass, rounded to milliseconds; still images have no timing.
    pub duration_ms: Option<u64>,
    /// Presence of a declared alpha auxiliary, not whether its pixels are opaque.
    pub has_alpha: Option<bool>,
    /// Declared AV1 chroma layout. Derived items may have no direct declaration.
    pub chroma_subsampling: Option<String>,
    pub bit_depth: u8,
    pub color_primaries: Option<u16>,
    pub transfer_characteristics: Option<u16>,
    pub matrix_coefficients: Option<u16>,
    pub full_range: Option<bool>,
    pub has_gain_map: bool,
    pub has_icc: bool,
    pub rotation_quarter_turns: u8,
    pub mirror_axis: Option<u8>,
    pub content_light_level: Option<ContentLightLevel>,
    pub mastering_display: Option<MasteringDisplay>,
}

/// Source `clli` values in cd/m². Zero means the source declares no upper bound;
/// an absent box is represented separately by `None` on AvifInfo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentLightLevel {
    pub max_content_light_level: u16,
    pub max_frame_average_light_level: u16,
}

/// Source `mdcv` values, not display capability or measured content brightness.
/// Chromaticities are CIE 1931 xy × 50000; luminances are cd/m² × 10000.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasteringDisplay {
    pub red_x: u16,
    pub red_y: u16,
    pub green_x: u16,
    pub green_y: u16,
    pub blue_x: u16,
    pub blue_y: u16,
    pub white_x: u16,
    pub white_y: u16,
    pub max_luminance: u32,
    pub min_luminance: u32,
}

#[derive(Clone, Copy)]
pub(super) struct BoxRef<'a> {
    pub kind: &'a [u8],
    pub data: &'a [u8],
    pub start: usize,
    pub header: usize,
}

pub(super) fn boxes(mut bytes: &[u8], mut offset: usize) -> Result<Vec<BoxRef<'_>>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        ensure!(out.len() < MAX_BOXES, "Too many AVIF boxes");
        ensure!(bytes.len() >= 8, "Truncated AVIF box");
        let short_size = be32(bytes, 0)?;
        let (size, header) = match short_size {
            0 => (bytes.len(), 8),
            1 => (usize::try_from(be64(bytes, 8)?)?, 16),
            n => (n as usize, 8),
        };
        ensure!(size >= header && size <= bytes.len(), "Invalid AVIF box size");
        out.push(BoxRef { kind: &bytes[4..8], data: &bytes[header..size], start: offset, header });
        bytes = &bytes[size..];
        offset += size;
    }
    Ok(out)
}

pub(super) fn be16(data: &[u8], i: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        data.get(i..i + 2).ok_or_else(|| anyhow!("Truncated AVIF field"))?.try_into()?,
    ))
}
pub(super) fn be32(data: &[u8], i: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(i..i + 4).ok_or_else(|| anyhow!("Truncated AVIF field"))?.try_into()?,
    ))
}
pub(super) fn be64(data: &[u8], i: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(
        data.get(i..i + 8).ok_or_else(|| anyhow!("Truncated AVIF field"))?.try_into()?,
    ))
}
fn tail(data: &[u8], from: usize) -> Result<&[u8]> {
    data.get(from..).ok_or_else(|| anyhow!("Truncated AVIF field"))
}
fn child<'a>(list: &[BoxRef<'a>], kind: &[u8; 4]) -> Result<BoxRef<'a>> {
    let mut matching = list.iter().filter(|b| b.kind == kind);
    let result = matching
        .next()
        .copied()
        .ok_or_else(|| anyhow!("Missing AVIF {} box", String::from_utf8_lossy(kind)))?;
    ensure!(matching.next().is_none(), "Duplicate AVIF box");
    Ok(result)
}
fn children(b: BoxRef<'_>, skip: usize) -> Result<Vec<BoxRef<'_>>> {
    boxes(tail(b.data, skip)?, b.start + b.header + skip)
}

/// Recognizes AVIF brands, bounds image/sequence resources, and reads metadata
/// associated with the primary image or colour track. No bytes are rewritten.
pub fn inspect_avif(bytes: &[u8]) -> Result<Option<AvifInfo>> {
    if bytes.get(4..8) != Some(b"ftyp".as_slice()) {
        return Ok(None);
    }
    // Read the declared ftyp independently before parsing an unrelated BMFF file.
    let size = be32(bytes, 0)? as usize;
    ensure!(size >= 16 && size <= bytes.len(), "Invalid AVIF file type box");
    let ftyp = &bytes[8..size];
    let is_avif = ftyp[..4] == *b"avif"
        || ftyp[..4] == *b"avis"
        || ftyp[8..].chunks_exact(4).any(|b| b == b"avif" || b == b"avis");
    if !is_avif {
        return Ok(None);
    }
    ensure!((ftyp.len() - 8).is_multiple_of(4), "Invalid AVIF brands");
    let top = boxes(bytes, 0)?;
    ensure!(top.iter().any(|b| b.kind == b"mdat" || b.kind == b"meta"), "Missing AVIF image data");
    let mut info = None;
    let mut gain_map = false;
    let mut decoded_pixel_count = 0;
    let mut max_coded_edge = 0;
    if let Some(meta) = top.iter().find(|b| b.kind == b"meta") {
        let parsed = inspect_meta(*meta, bytes.len())?;
        gain_map = parsed.has_gain_map;
        decoded_pixel_count = parsed.decoded_pixel_count;
        max_coded_edge = parsed.max_coded_edge;
        info = Some(parsed);
    }
    if let Some(moov) = top.iter().find(|b| b.kind == b"moov") {
        let tracks = children(*moov, 0)?;
        let mut inspected = Vec::new();
        let mut track_pixels = 0u64;
        for trak in tracks.iter().filter(|b| b.kind == b"trak") {
            let track = inspect_track(*trak, bytes.len())?;
            track_pixels = track_pixels
                .checked_add(track.info.decoded_pixel_count)
                .ok_or_else(|| anyhow!("AVIF decoded pixel count overflow"))?;
            max_coded_edge = max_coded_edge.max(track.info.max_coded_edge);
            inspected.push(track);
        }
        let colour_index = inspected
            .iter()
            .position(|t| t.is_colour && t.aux_for.is_empty())
            .ok_or_else(|| anyhow!("AVIF sequence has no colour track"))?;
        let colour_id = inspected[colour_index].id;
        let mut has_alpha = Some(false);
        for auxiliary in inspected.iter().filter(|t| t.aux_for.contains(&colour_id)) {
            match auxiliary.alpha_type {
                Some(true) => {
                    has_alpha = Some(true);
                    break;
                },
                None => has_alpha = None, // An untyped legacy auxiliary is not an alpha declaration.
                Some(false) => {},
            }
        }
        let mut colour = inspected.swap_remove(colour_index).info;
        colour.has_alpha = has_alpha;
        // The decoder selects item or track decoding; do not charge the same
        // first-frame payload twice just because it is also a primary item.
        decoded_pixel_count = decoded_pixel_count.max(track_pixels);
        info = Some(colour);
    }
    let mut info = info.ok_or_else(|| anyhow!("Missing AVIF image description"))?;
    info.has_gain_map |= gain_map;
    info.decoded_pixel_count = decoded_pixel_count;
    info.max_coded_edge = max_coded_edge;
    validate_dimensions(info.width, info.height)?;
    ensure!(matches!(info.bit_depth, 8 | 10 | 12), "Missing or unsupported AVIF bit depth");
    Ok(Some(info))
}

fn inspect_meta(meta: BoxRef<'_>, file_len: usize) -> Result<AvifInfo> {
    let list = children(meta, 4)?;
    let pitm = child(&list, b"pitm")?;
    let primary = match pitm.data.first() {
        Some(0) => u32::from(be16(pitm.data, 4)?),
        Some(1) => be32(pitm.data, 4)?,
        _ => bail!("Unsupported AVIF primary item version"),
    };
    let iprp = children(child(&list, b"iprp")?, 0)?;
    let props = children(child(&iprp, b"ipco")?, 0)?;
    // Bound every coded auxiliary image too, not just the displayed primary.
    let mut max_coded_edge = 0;
    let mut largest_image_pixels = 0;
    for prop in &props {
        if prop.kind == b"ispe" {
            let (width, height) = (be32(prop.data, 4)?, be32(prop.data, 8)?);
            validate_dimensions(width, height)?;
            max_coded_edge = max_coded_edge.max(width.max(height));
            largest_image_pixels = largest_image_pixels.max(u64::from(width) * u64::from(height));
        }
    }
    let ipma = child(&iprp, b"ipma")?;
    ensure!(ipma.data.len() >= 8 && ipma.data[0] <= 1, "Invalid AVIF associations");
    let wide = ipma.data[3] & 1 != 0;
    let count = be32(ipma.data, 4)?;
    ensure!(count <= MAX_BOXES as u32, "Too many AVIF items");
    let mut pos = 8;
    let mut selected = Vec::new();
    let mut decoded_pixel_count = 0u64;
    let mut auxiliary_types = BTreeMap::new();
    for _ in 0..count {
        let id = if ipma.data[0] == 0 {
            let v = u32::from(be16(ipma.data, pos)?);
            pos += 2;
            v
        } else {
            let v = be32(ipma.data, pos)?;
            pos += 4;
            v
        };
        let n = *ipma.data.get(pos).ok_or_else(|| anyhow!("Truncated AVIF associations"))?;
        pos += 1;
        let mut item_pixels = 0;
        let mut coded = false;
        for _ in 0..n {
            let index = if wide {
                let v = be16(ipma.data, pos)? & 0x7fff;
                pos += 2;
                v as usize
            } else {
                let v =
                    *ipma.data.get(pos).ok_or_else(|| anyhow!("Truncated AVIF associations"))?
                        & 0x7f;
                pos += 1;
                v as usize
            };
            ensure!(index <= props.len(), "AVIF property index out of range");
            if index != 0 {
                let prop = props[index - 1];
                if prop.kind == b"ispe" {
                    item_pixels = item_pixels
                        .max(u64::from(be32(prop.data, 4)?) * u64::from(be32(prop.data, 8)?));
                }
                coded |= prop.kind == b"av1C";
                if prop.kind == b"auxC" {
                    auxiliary_types.insert(id, auxiliary_is_alpha(prop)?);
                }
                if id == primary {
                    selected.push(prop);
                }
            }
        }
        // Properties can be shared across distinct colour/alpha/grid items.
        // Charge each item, not each unique ispe. Legacy alpha items may omit
        // ispe, in which case their dimensions follow the colour image.
        if item_pixels == 0 && coded {
            item_pixels = largest_image_pixels;
        }
        decoded_pixel_count = decoded_pixel_count
            .checked_add(item_pixels)
            .ok_or_else(|| anyhow!("AVIF decoded pixel count overflow"))?;
    }
    ensure!(pos == ipma.data.len(), "Trailing AVIF associations");
    let mut info = properties(&selected, AvifInfo::default())?;
    let refs = item_references(&list)?;
    info.has_alpha =
        item_has_alpha(primary, &auxiliary_types, &refs, &mut Vec::new(), &mut BTreeMap::new())?;
    info.frame_count = 1;
    info.decoded_pixel_count = decoded_pixel_count.max(largest_image_pixels);
    info.max_coded_edge = max_coded_edge;
    if let Some(iinf) = list.iter().find(|b| b.kind == b"iinf") {
        let skip = if iinf.data.first() == Some(&0) { 6 } else { 8 };
        for item in children(*iinf, skip)? {
            if item.kind != b"infe" {
                continue;
            }
            let type_pos = match item.data.first() {
                Some(2) => 8,
                Some(3) => 10,
                _ => continue,
            };
            info.has_gain_map |= item.data.get(type_pos..type_pos + 4) == Some(b"tmap".as_slice());
        }
    }
    validate_locations(child(&list, b"iloc")?, &list, file_len)?;
    Ok(info)
}

fn auxiliary_is_alpha(prop: BoxRef<'_>) -> Result<bool> {
    ensure!(prop.data.first() == Some(&0), "Unsupported AVIF auxiliary type version");
    let value = tail(prop.data, 4)?;
    let end = value
        .iter()
        .take(256)
        .position(|b| *b == 0)
        .ok_or_else(|| anyhow!("Invalid AVIF auxiliary type string"))?;
    Ok(matches!(
        &value[..end],
        b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha" | b"urn:mpeg:hevc:2015:auxid:1"
    ))
}

struct ItemReference {
    kind: [u8; 4],
    from: u32,
    to: Vec<u32>,
}

fn item_references(list: &[BoxRef<'_>]) -> Result<Vec<ItemReference>> {
    let Some(iref) = list.iter().find(|b| b.kind == b"iref") else { return Ok(Vec::new()) };
    let id_bytes = match iref.data.first() {
        Some(0) => 2,
        Some(1) => 4,
        _ => bail!("Unsupported AVIF item reference version"),
    };
    let mut refs = Vec::new();
    let mut total = 0;
    for entry in children(*iref, 4)? {
        let id = |pos| -> Result<u32> {
            if id_bytes == 2 {
                Ok(u32::from(be16(entry.data, pos)?))
            } else {
                be32(entry.data, pos)
            }
        };
        let from = id(0)?;
        let count = usize::from(be16(entry.data, id_bytes)?);
        total += count;
        ensure!(
            total <= MAX_BOXES && entry.data.len() == id_bytes + 2 + count * id_bytes,
            "Invalid AVIF item references"
        );
        if entry.kind != b"auxl" && entry.kind != b"dimg" {
            continue;
        }
        let to = (0..count).map(|i| id(id_bytes + 2 + i * id_bytes)).collect::<Result<Vec<_>>>()?;
        refs.push(ItemReference { kind: entry.kind.try_into()?, from, to });
    }
    Ok(refs)
}

fn item_has_alpha(
    id: u32, types: &BTreeMap<u32, bool>, refs: &[ItemReference], path: &mut Vec<u32>,
    memo: &mut BTreeMap<u32, Option<bool>>,
) -> Result<Option<bool>> {
    if let Some(value) = memo.get(&id) {
        return Ok(*value);
    }
    ensure!(
        path.len() < 16 && !path.contains(&id),
        "Cyclic or excessively nested AVIF image references"
    );
    path.push(id);
    let mut uncertain = false;
    for auxiliary in refs.iter().filter(|r| r.kind == *b"auxl" && r.to.contains(&id)) {
        match types.get(&auxiliary.from) {
            Some(true) => {
                path.pop();
                memo.insert(id, Some(true));
                return Ok(Some(true));
            },
            None => uncertain = true,
            Some(false) => {},
        }
    }
    let (mut children, mut all_alpha, mut all_opaque) = (0, true, true);
    for derived in refs.iter().filter(|r| r.kind == *b"dimg" && r.from == id) {
        for child in &derived.to {
            let alpha = item_has_alpha(*child, types, refs, path, memo)?;
            all_alpha &= alpha == Some(true);
            all_opaque &= alpha == Some(false);
            children += 1;
        }
    }
    path.pop();
    // Grid alpha may be represented by an auxiliary on every colour tile. A
    // partial/mixed declaration cannot establish alpha for the composed image.
    let value = if uncertain {
        None
    } else if children == 0 || all_opaque {
        Some(false)
    } else if all_alpha {
        Some(true)
    } else {
        None
    };
    memo.insert(id, value);
    Ok(value)
}

fn properties(props: &[BoxRef<'_>], mut info: AvifInfo) -> Result<AvifInfo> {
    for prop in props {
        if prop.kind == b"ispe" {
            info.width = be32(prop.data, 4)?;
            info.height = be32(prop.data, 8)?;
            validate_dimensions(info.width, info.height)?;
            info.decoded_pixel_count =
                info.decoded_pixel_count.max(u64::from(info.width) * u64::from(info.height));
            info.max_coded_edge = info.max_coded_edge.max(info.width.max(info.height));
        }
    }
    validate_dimensions(info.width, info.height)?;
    info.decoded_pixel_count =
        info.decoded_pixel_count.max(u64::from(info.width) * u64::from(info.height));
    info.max_coded_edge = info.max_coded_edge.max(info.width.max(info.height));
    for prop in props {
        match prop.kind {
            b"ispe" => {},
            b"pixi" if info.bit_depth == 0 => {
                ensure!(prop.data.len() >= 6 && prop.data[4] > 0, "Invalid AVIF pixel information");
                info.bit_depth = prop.data[5];
            },
            b"av1C" => {
                ensure!(prop.data.len() >= 4 && prop.data[0] == 0x81, "Invalid AV1 configuration");
                info.bit_depth = if prop.data[2] & 0x40 == 0 {
                    8
                } else if prop.data[2] & 0x20 == 0 {
                    10
                } else {
                    12
                };
                info.chroma_subsampling = match (prop.data[2] & 0x10 != 0, prop.data[2] & 0x0c) {
                    (true, _) => Some("4:0:0"),
                    (false, 0x00) => Some("4:4:4"),
                    (false, 0x08) => Some("4:2:2"),
                    (false, 0x0c) => Some("4:2:0"),
                    _ => None,
                }
                .map(str::to_owned);
            },
            b"clli" => {
                ensure!(prop.data.len() == 4, "Invalid AVIF content light metadata size");
                ensure!(
                    info.content_light_level.is_none(),
                    "Duplicate AVIF content light metadata"
                );
                info.content_light_level = Some(ContentLightLevel {
                    max_content_light_level: be16(prop.data, 0)?,
                    max_frame_average_light_level: be16(prop.data, 2)?,
                });
            },
            b"mdcv" => {
                // ISOBMFF mdcv is a Box (no version/flags), with G, B, R
                // primaries. Its scales differ from AV1's HDR_MDCV OBU.
                ensure!(prop.data.len() == 24, "Invalid AVIF mastering display metadata size");
                ensure!(
                    info.mastering_display.is_none(),
                    "Duplicate AVIF mastering display metadata"
                );
                info.mastering_display = Some(MasteringDisplay {
                    green_x: be16(prop.data, 0)?,
                    green_y: be16(prop.data, 2)?,
                    blue_x: be16(prop.data, 4)?,
                    blue_y: be16(prop.data, 6)?,
                    red_x: be16(prop.data, 8)?,
                    red_y: be16(prop.data, 10)?,
                    white_x: be16(prop.data, 12)?,
                    white_y: be16(prop.data, 14)?,
                    max_luminance: be32(prop.data, 16)?,
                    min_luminance: be32(prop.data, 20)?,
                });
            },
            b"colr" => match prop.data.get(..4) {
                Some(b"nclx") => {
                    info.color_primaries = Some(be16(prop.data, 4)?);
                    info.transfer_characteristics = Some(be16(prop.data, 6)?);
                    info.matrix_coefficients = Some(be16(prop.data, 8)?);
                    info.full_range = Some(
                        *prop
                            .data
                            .get(10)
                            .ok_or_else(|| anyhow!("Truncated AVIF colour information"))?
                            & 0x80
                            != 0,
                    );
                },
                Some(b"prof" | b"rICC") => info.has_icc = true,
                _ => {},
            },
            b"irot" => {
                info.rotation_quarter_turns =
                    prop.data.first().ok_or_else(|| anyhow!("Truncated AVIF rotation"))? & 3
            },
            b"imir" => {
                info.mirror_axis =
                    Some(prop.data.first().ok_or_else(|| anyhow!("Truncated AVIF reflection"))? & 1)
            },
            b"clap" => {
                // A clean aperture can only shrink the coded image. Fractional
                // display sizes are rounded up for conservative resource limits.
                let (w, wd, h, hd) = (
                    be32(prop.data, 0)?,
                    be32(prop.data, 4)?,
                    be32(prop.data, 8)?,
                    be32(prop.data, 12)?,
                );
                ensure!(wd != 0 && hd != 0 && prop.data.len() >= 32, "Invalid AVIF clean aperture");
                let (w, h) = (w.div_ceil(wd), h.div_ceil(hd));
                ensure!(
                    w > 0 && h > 0 && w <= info.width && h <= info.height,
                    "Invalid AVIF clean aperture size"
                );
                info.width = w;
                info.height = h;
            },
            _ => {},
        }
    }
    if info.rotation_quarter_turns & 1 != 0 {
        std::mem::swap(&mut info.width, &mut info.height);
    }
    Ok(info)
}

fn validate_locations(iloc: BoxRef<'_>, siblings: &[BoxRef<'_>], file_len: usize) -> Result<()> {
    let d = iloc.data;
    ensure!(d.len() >= 8 && d[0] <= 2, "Invalid AVIF item locations");
    let (off, len, base, index) = (
        (d[4] >> 4) as usize,
        (d[4] & 15) as usize,
        (d[5] >> 4) as usize,
        if d[0] == 0 { 0 } else { (d[5] & 15) as usize },
    );
    ensure!([off, len, base, index].into_iter().all(|n| n <= 8), "Invalid AVIF offset width");
    let mut pos = 6;
    let count = if d[0] < 2 {
        let n = be16(d, pos)? as u32;
        pos += 2;
        n
    } else {
        let n = be32(d, pos)?;
        pos += 4;
        n
    };
    ensure!(count <= MAX_BOXES as u32, "Too many AVIF image items");
    let read = |pos: &mut usize, size: usize| -> Result<u64> {
        let value = d
            .get(*pos..*pos + size)
            .ok_or_else(|| anyhow!("Truncated AVIF item offset"))?
            .iter()
            .fold(0u64, |v, b| (v << 8) | u64::from(*b));
        *pos += size;
        Ok(value)
    };
    for _ in 0..count {
        read(&mut pos, if d[0] < 2 { 2 } else { 4 })?;
        let method = if d[0] > 0 { read(&mut pos, 2)? & 15 } else { 0 };
        ensure!(read(&mut pos, 2)? == 0, "External AVIF image references are unsupported");
        let base_offset = read(&mut pos, base)?;
        let extents = read(&mut pos, 2)?;
        ensure!(extents <= MAX_BOXES as u64, "Too many AVIF extents");
        let bound = match method {
            0 => file_len as u64,
            1 => child(siblings, b"idat")?.data.len() as u64,
            _ => bail!("Unsupported AVIF item construction"),
        };
        for _ in 0..extents {
            read(&mut pos, index)?;
            let offset = read(&mut pos, off)?;
            let length = read(&mut pos, len)?;
            ensure!(
                base_offset
                    .checked_add(offset)
                    .and_then(|p| p.checked_add(length))
                    .is_some_and(|end| end <= bound),
                "AVIF image data is outside the file"
            );
        }
    }
    ensure!(pos == d.len(), "Trailing AVIF item locations");
    Ok(())
}

struct TrackInspection {
    info: AvifInfo,
    id: u32,
    is_colour: bool,
    aux_for: Vec<u32>,
    alpha_type: Option<bool>,
}

fn inspect_track(trak: BoxRef<'_>, file_len: usize) -> Result<TrackInspection> {
    let trak = children(trak, 0)?;
    let tkhd = child(&trak, b"tkhd")?;
    let id = be32(
        tkhd.data,
        match tkhd.data.first() {
            Some(0) => 12,
            Some(1) => 20,
            _ => bail!("Unsupported AVIF track header version"),
        },
    )?;
    let mut aux_for = Vec::new();
    if let Some(tref) = trak.iter().find(|b| b.kind == b"tref") {
        for reference in children(*tref, 0)? {
            if reference.kind != b"auxl" {
                continue;
            }
            ensure!(
                reference.data.len() % 4 == 0 && reference.data.len() / 4 <= MAX_BOXES,
                "Invalid AVIF auxiliary track reference"
            );
            for pos in (0..reference.data.len()).step_by(4) {
                aux_for.push(be32(reference.data, pos)?);
            }
            ensure!(aux_for.len() <= MAX_BOXES, "Too many AVIF auxiliary track references");
        }
    }
    let mdia = children(child(&trak, b"mdia")?, 0)?;
    let handler = child(&mdia, b"hdlr")?;
    let is_colour = handler.data.get(8..12) == Some(b"pict".as_slice())
        || handler.data.get(8..12) == Some(b"vide".as_slice());
    let minf = children(child(&mdia, b"minf")?, 0)?;
    let stbl = children(child(&minf, b"stbl")?, 0)?;
    let stsd = child(&stbl, b"stsd")?;
    ensure!(be32(stsd.data, 4)? == 1, "Unsupported AVIF sample descriptions");
    let descriptions = children(stsd, 8)?;
    let sample = child(&descriptions, b"av01")?;
    let info = AvifInfo {
        width: u32::from(be16(sample.data, 24)?),
        height: u32::from(be16(sample.data, 26)?),
        ..Default::default()
    };
    validate_dimensions(info.width, info.height)?;
    // Preserve sample-entry allocation dimensions even if a later property
    // gives a smaller display canvas or clean aperture.
    let info = AvifInfo {
        decoded_pixel_count: u64::from(info.width) * u64::from(info.height),
        max_coded_edge: info.width.max(info.height),
        ..info
    };
    let sample_props = children(sample, 78)?;
    let alpha_type = sample_props
        .iter()
        .find(|p| p.kind == b"auxi")
        .map(|p| auxiliary_is_alpha(*p))
        .transpose()?;
    let mut info = properties(&sample_props, info)?;
    let stsz = child(&stbl, b"stsz")?;
    let sample_count = be32(stsz.data, 8)?;
    ensure!(
        sample_count > 0 && sample_count <= MAX_FRAMES,
        "AVIF animations can contain at most {MAX_FRAMES} frames"
    );
    if be32(stsz.data, 4)? == 0 {
        ensure!(stsz.data.len() == 12 + sample_count as usize * 4, "Invalid AVIF sample sizes");
        for i in 0..sample_count as usize {
            ensure!(be32(stsz.data, 12 + i * 4)? as usize <= file_len, "Invalid AVIF sample size");
        }
    }
    let stts = child(&stbl, b"stts")?;
    let timing_count = be32(stts.data, 4)? as usize;
    ensure!(
        timing_count <= MAX_FRAMES as usize && stts.data.len() == 8 + timing_count * 8,
        "Invalid AVIF frame timing"
    );
    let mut frames = 0u64;
    let mut duration = 0u64;
    for i in 0..timing_count {
        let count = u64::from(be32(stts.data, 8 + i * 8)?);
        let delta = u64::from(be32(stts.data, 12 + i * 8)?);
        ensure!(delta > 0, "AVIF frame duration is zero");
        frames = frames.checked_add(count).ok_or_else(|| anyhow!("AVIF frame count overflow"))?;
        duration =
            duration.checked_add(count * delta).ok_or_else(|| anyhow!("AVIF duration overflow"))?;
    }
    ensure!(frames == u64::from(sample_count), "AVIF frame tables disagree");
    let mdhd = child(&mdia, b"mdhd")?;
    let timescale = be32(
        mdhd.data,
        match mdhd.data.first() {
            Some(0) => 12,
            Some(1) => 20,
            _ => bail!("Invalid AVIF media header"),
        },
    )?;
    ensure!(
        timescale > 0 && duration <= u64::from(timescale) * (MAX_DURATION_CS / 100),
        "AVIF animation exceeds five minutes"
    );
    for chunk in stbl.iter().filter(|b| b.kind == b"stco" || b.kind == b"co64") {
        let count = be32(chunk.data, 4)? as usize;
        let width = if chunk.kind == b"stco" { 4 } else { 8 };
        ensure!(
            count <= MAX_FRAMES as usize && chunk.data.len() == 8 + count * width,
            "Invalid AVIF chunks"
        );
        for i in 0..count {
            let offset = if width == 4 {
                u64::from(be32(chunk.data, 8 + i * width)?)
            } else {
                be64(chunk.data, 8 + i * width)?
            };
            ensure!(offset < file_len as u64, "AVIF chunk is outside the file");
        }
    }
    info.animated = sample_count > 1;
    info.frame_count = sample_count;
    info.duration_ms = Some((duration * 1000 + u64::from(timescale) / 2) / u64::from(timescale));
    Ok(TrackInspection { info, id, is_colour, aux_for, alpha_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_truncated_or_ambiguous_hdr_metadata_without_inventing_values() {
        let base = || AvifInfo { width: 16, height: 16, ..Default::default() };
        for (kind, size) in [(b"clli".as_slice(), 4), (b"mdcv".as_slice(), 24)] {
            for length in (0..size).chain(std::iter::once(size + 1)) {
                let data = vec![0u8; length];
                assert!(
                    properties(&[BoxRef { kind, data: &data, start: 0, header: 8 }], base())
                        .is_err()
                );
            }
            let data = vec![0u8; size];
            let prop = BoxRef { kind, data: &data, start: 0, header: 8 };
            assert!(properties(&[prop, prop], base()).is_err());
        }
        let missing = properties(&[], base()).unwrap();
        assert!(missing.content_light_level.is_none());
        assert!(missing.mastering_display.is_none());
        let declared = properties(
            &[BoxRef { kind: b"clli", data: &[0, 0, 0, 0], start: 0, header: 8 }],
            base(),
        )
        .unwrap();
        assert_eq!(
            declared.content_light_level,
            Some(ContentLightLevel {
                max_content_light_level: 0,
                max_frame_average_light_level: 0,
            })
        );
    }

    #[test]
    fn malicious_alpha_reference_cycles_are_bounded() {
        let types = BTreeMap::new();
        let refs = vec![
            ItemReference { kind: *b"dimg", from: 1, to: vec![2] },
            ItemReference { kind: *b"dimg", from: 2, to: vec![1] },
        ];
        assert!(item_has_alpha(1, &types, &refs, &mut Vec::new(), &mut BTreeMap::new()).is_err());
        let refs = (1..=17)
            .map(|id| ItemReference { kind: *b"dimg", from: id, to: vec![id + 1] })
            .collect::<Vec<_>>();
        assert!(item_has_alpha(1, &types, &refs, &mut Vec::new(), &mut BTreeMap::new()).is_err());
    }

    #[test]
    fn malformed_avif_never_panics_or_bypasses_dimension_limit() {
        let pixels = vec![0xff; 16 * 8 * 4];
        let original = crate::media::encode_avif(&pixels, 16, 8, 60.0).unwrap();
        let info = inspect_avif(&original).unwrap().unwrap();
        assert_eq!((info.width, info.height, info.bit_depth), (16, 8, 8));
        for end in 8..original.len() {
            let _ = inspect_avif(&original[..end]);
        }
        let mut bomb = original.clone();
        let ispe = bomb.windows(4).position(|b| b == b"ispe").unwrap();
        bomb[ispe + 8..ispe + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(inspect_avif(&bomb).is_err());
        let mut overrun = original;
        let iloc = overrun.windows(4).position(|b| b == b"iloc").unwrap();
        // Self-generated v0 iloc: first extent offset follows its one-item header.
        overrun[iloc + 18..iloc + 22].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(inspect_avif(&overrun).is_err());
    }

    #[test]
    fn preserve_source_does_not_rewrite_colour_or_unknown_metadata() {
        let rgba = vec![120; 8 * 8 * 4];
        let mut source = crate::media::encode_avif(&rgba, 8, 8, 60.0).unwrap();
        source.extend_from_slice(&[0, 0, 0, 12, b'f', b'r', b'e', b'e', 9, 8, 7, 6]);
        let prepared = crate::media::process_encoded_image(
            &source,
            crate::media::MediaPolicy { max_bytes: source.len(), max_edge: 8 },
        )
        .unwrap()
        .unwrap();
        assert_eq!(prepared.bytes, source);
        assert!(
            crate::media::process_encoded_image(
                &source,
                crate::media::MediaPolicy { max_bytes: source.len() - 1, max_edge: 8 }
            )
            .is_err()
        );
    }

    #[test]
    fn clean_aperture_cannot_hide_coded_image_or_auxiliary_allocation() {
        // A crafted container advertises a 4096px colour+alpha image cropped to
        // a 512px sticker. Both images share ispe, as ordinary ravif output does.
        let rgba = vec![120; 16 * 16 * 4];
        let mut source = crate::media::encode_avif(&rgba, 16, 16, 60.0).unwrap();
        let top = boxes(&source, 0).unwrap();
        let meta = child(&top, b"meta").unwrap();
        let meta_children = children(meta, 4).unwrap();
        let iprp = child(&meta_children, b"iprp").unwrap();
        let iprp_children = children(iprp, 0).unwrap();
        let ipco = child(&iprp_children, b"ipco").unwrap();
        let props = children(ipco, 0).unwrap();
        let ispe = child(&props, b"ispe").unwrap();
        let ipma = child(&iprp_children, b"ipma").unwrap();
        let iloc = child(&meta_children, b"iloc").unwrap();

        let meta_start = meta.start;
        let iprp_start = iprp.start;
        let ipco_start = ipco.start;
        let ipma_start = ipma.start;
        let property_end = ipco.start + ipco.header + ipco.data.len();
        let dimension_pos = ispe.start + ispe.header + 4;
        let association_count_pos = ipma.start + ipma.header + 10;
        let association_end =
            association_count_pos + 1 + usize::from(source[association_count_pos]);
        let property_index = (props.len() + 1) as u8;
        let mut offsets = Vec::new();
        for item in 0..be16(iloc.data, 6).unwrap() as usize {
            let pos = iloc.start + iloc.header + 14 + item * 14;
            offsets.push((pos, be32(&source, pos).unwrap()));
        }
        drop(top);

        let put32 = |data: &mut Vec<u8>, pos: usize, value: u32| {
            data[pos..pos + 4].copy_from_slice(&value.to_be_bytes());
        };
        put32(&mut source, dimension_pos, 4096);
        put32(&mut source, dimension_pos + 4, 4096);
        for (pos, offset) in offsets {
            put32(&mut source, pos, offset + 41);
        }
        for (pos, growth) in [(meta_start, 41), (iprp_start, 41), (ipco_start, 40), (ipma_start, 1)]
        {
            let size = be32(&source, pos).unwrap();
            put32(&mut source, pos, size + growth);
        }
        source[association_count_pos] += 1;
        source.insert(association_end, property_index | 0x80);
        let mut clap = Vec::from(40u32.to_be_bytes());
        clap.extend_from_slice(b"clap");
        for value in [512u32, 1, 512, 1, 0, 1, 0, 1] {
            clap.extend_from_slice(&value.to_be_bytes());
        }
        source.splice(property_end..property_end, clap);

        let info = inspect_avif(&source).unwrap().unwrap();
        assert_eq!((info.width, info.height), (512, 512));
        assert_eq!(info.max_coded_edge, 4096);
        assert_eq!(info.decoded_pixel_count, 2 * 4096 * 4096);
        assert!(
            crate::media::process_encoded_image(
                &source,
                crate::media::MediaPolicy { max_bytes: 256 * 1024, max_edge: 512 }
            )
            .is_err()
        );
    }
}
