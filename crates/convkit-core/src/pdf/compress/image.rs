//! Which images conv changes and how: who is eligible, the three rules a
//! level applies to each image, and running them over many images at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Serialize;

use super::codec::{self, Pixels, Space};
use super::walk::{dict_of, resolve, Placements};
use super::Level;

/// Why an image is left as it is. The serialised name is its key in
/// `--json`'s `images_skipped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// Black and white: CCITT, JBIG2, a stencil mask, or any 1-bit image.
    /// Already compact.
    OneBit,
    /// JPEG 2000. Already compact, and conv has no decoder for it.
    Jpeg2000,
    /// 2, 4 or 16 bits a sample.
    BitDepth,
    /// Indexed, Lab, Separation, DeviceN, or CMYK that is not a JPEG with
    /// an Adobe marker.
    ColorSpace,
    /// LZW, run-length, or a JPEG with decode parameters.
    Filter,
    /// A `/Mask`, or a soft mask conv cannot resize with it.
    Mask,
    /// A `/Decode` array on a gray or RGB image.
    DecodeArray,
    /// Listed in a page's resources but never drawn.
    NotDrawn,
    /// Data that does not decode, or a page whose content conv could not
    /// read, so where it draws the image is unknown.
    Unreadable,
}

/// How an image's data is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coding {
    /// A JPEG; `inflate` when a copier wrapped it in Flate as well
    /// (`[/FlateDecode /DCTDecode]`), a layer the rewrite drops.
    Jpeg { inflate: bool },
    /// Samples, raw or Flate (with or without a predictor).
    Samples,
}

/// An image's soft mask, resized with it and kept lossless.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SoftMask {
    pub id: ObjectId,
    pub width: u32,
    pub height: u32,
    pub coding: Coding,
    pub bytes: u64,
    /// Another image uses it too, so resizing it for one would break the
    /// other: the image is then never downsampled.
    pub shared: bool,
}

/// An image the rules may change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub id: ObjectId,
    pub width: u32,
    pub height: u32,
    pub space: Space,
    pub coding: Coding,
    /// The lowest resolution it is drawn at.
    pub dpi: f64,
    /// The image's stored bytes, its soft mask's not included.
    pub bytes: u64,
    pub mask: Option<SoftMask>,
}

/// One image a page lists, and what conv makes of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Surveyed {
    pub id: ObjectId,
    /// Stored bytes, its soft mask's included.
    pub bytes: u64,
    pub dpi: Option<f64>,
    pub verdict: Result<Candidate, Skip>,
}

fn int(d: &Dictionary, key: &[u8]) -> Option<i64> {
    d.get(key).ok().and_then(|o| o.as_i64().ok())
}

/// The filter names of a stream, in decoding order.
fn filters(doc: &Document, d: &Dictionary) -> Vec<Vec<u8>> {
    match d.get(b"Filter").ok().and_then(|o| resolve(doc, o)) {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a
            .iter()
            .filter_map(|o| {
                resolve(doc, o)
                    .and_then(|o| o.as_name().ok())
                    .map(<[u8]>::to_vec)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Gray, RGB or CMYK, from a device space or an ICC profile's component
/// count; anything else is `None`.
fn space_of(doc: &Document, cs: Option<&Object>) -> Option<Space> {
    match cs.and_then(|o| resolve(doc, o))? {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" => Some(Space::Gray),
            b"DeviceRGB" => Some(Space::Rgb),
            b"DeviceCMYK" => Some(Space::Cmyk),
            _ => None,
        },
        Object::Array(a) => {
            let [family, profile] = a.as_slice() else {
                return None;
            };
            if resolve(doc, family)?.as_name().ok()? != b"ICCBased" {
                return None;
            }
            match int(dict_of(doc, profile)?, b"N")? {
                1 => Some(Space::Gray),
                3 => Some(Space::Rgb),
                4 => Some(Space::Cmyk),
                _ => None,
            }
        }
        _ => None,
    }
}

/// How the stream is coded, when it is a way conv reads.
fn coding_of(doc: &Document, d: &Dictionary) -> Result<Coding, Skip> {
    let f = filters(doc, d);
    let names: Vec<&[u8]> = f.iter().map(Vec::as_slice).collect();
    let has_params = d
        .get(b"DecodeParms")
        .ok()
        .and_then(|o| resolve(doc, o))
        .is_some_and(|o| match o {
            Object::Null => false,
            Object::Array(a) => a
                .iter()
                .any(|p| !matches!(resolve(doc, p), Some(Object::Null) | None)),
            _ => true,
        });
    match names.as_slice() {
        [] | [b"FlateDecode"] => Ok(Coding::Samples),
        [b"DCTDecode"] if !has_params => Ok(Coding::Jpeg { inflate: false }),
        [b"FlateDecode", b"DCTDecode"] if !has_params => Ok(Coding::Jpeg { inflate: true }),
        _ => Err(Skip::Filter),
    }
}

fn image_stream(doc: &Document, id: ObjectId) -> Option<&Stream> {
    doc.get_object(id).ok()?.as_stream().ok()
}

/// A JPEG image's JPEG bytes: its data, inflated first under a Flate layer.
fn jpeg_bytes(stream: &Stream, inflate: bool, limit: usize) -> Option<Vec<u8>> {
    if !inflate {
        return Some(stream.content.clone());
    }
    let mut d = flate2::read::ZlibDecoder::new(stream.content.as_slice());
    let mut out = Vec::new();
    use std::io::Read;
    (&mut d).take(limit as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() <= limit).then_some(out)
}

/// The soft mask, when conv can resize it with its image.
fn soft_mask(
    doc: &Document,
    o: &Object,
    users: &HashMap<ObjectId, usize>,
) -> Result<SoftMask, Skip> {
    let id = o.as_reference().map_err(|_| Skip::Mask)?;
    let stream = image_stream(doc, id).ok_or(Skip::Mask)?;
    let d = &stream.dict;
    let gray = space_of(doc, d.get(b"ColorSpace").ok()).unwrap_or(Space::Gray) == Space::Gray;
    let usable = gray
        && int(d, b"BitsPerComponent") == Some(8)
        && d.get(b"Matte").is_err()
        && d.get(b"Decode").is_err();
    if !usable {
        return Err(Skip::Mask);
    }
    let coding = coding_of(doc, d).map_err(|_| Skip::Mask)?;
    let (width, height) = dims(d).ok_or(Skip::Mask)?;
    Ok(SoftMask {
        id,
        width,
        height,
        coding,
        bytes: stream.content.len() as u64,
        shared: users.get(&id).copied().unwrap_or(0) > 1,
    })
}

fn dims(d: &Dictionary) -> Option<(u32, u32)> {
    let w = u32::try_from(int(d, b"Width")?).ok()?;
    let h = u32::try_from(int(d, b"Height")?).ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// How many images use each soft mask.
pub(crate) fn mask_users(doc: &Document, images: &[ObjectId]) -> HashMap<ObjectId, usize> {
    let mut users = HashMap::new();
    for &id in images {
        if let Some(m) = image_stream(doc, id)
            .and_then(|s| s.dict.get(b"SMask").ok())
            .and_then(|o| o.as_reference().ok())
        {
            *users.entry(m).or_insert(0) += 1;
        }
    }
    users
}

/// What conv makes of image `id`.
pub(crate) fn survey(
    doc: &Document,
    id: ObjectId,
    placements: &Placements,
    users: &HashMap<ObjectId, usize>,
) -> Surveyed {
    let dpi = placements.dpi.get(&id).copied();
    let Some(stream) = image_stream(doc, id) else {
        return Surveyed {
            id,
            bytes: 0,
            dpi,
            verdict: Err(Skip::Unreadable),
        };
    };
    let d = &stream.dict;
    let mask_bytes = d
        .get(b"SMask")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|m| image_stream(doc, m))
        .map_or(0, |s| s.content.len() as u64);
    let image_bytes = stream.content.len() as u64;
    Surveyed {
        id,
        bytes: image_bytes + mask_bytes,
        dpi,
        verdict: classify(doc, id, stream, dpi, placements, users),
    }
}

fn classify(
    doc: &Document,
    id: ObjectId,
    stream: &Stream,
    dpi: Option<f64>,
    placements: &Placements,
    users: &HashMap<ObjectId, usize>,
) -> Result<Candidate, Skip> {
    let d = &stream.dict;
    if d.get(b"ImageMask")
        .ok()
        .and_then(|o| o.as_bool().ok())
        .unwrap_or(false)
    {
        return Err(Skip::OneBit);
    }
    let f = filters(doc, d);
    let has = |name: &[u8]| f.iter().any(|n| n == name);
    if has(b"JPXDecode") {
        return Err(Skip::Jpeg2000);
    }
    if has(b"CCITTFaxDecode") || has(b"JBIG2Decode") {
        return Err(Skip::OneBit);
    }
    match int(d, b"BitsPerComponent") {
        Some(1) => return Err(Skip::OneBit),
        Some(8) => {}
        _ => return Err(Skip::BitDepth),
    }
    if placements.unknown.contains(&id) {
        return Err(Skip::Unreadable);
    }
    let dpi = dpi.ok_or(Skip::NotDrawn)?;
    if d.get(b"Mask").is_ok() {
        return Err(Skip::Mask);
    }
    let space = space_of(doc, d.get(b"ColorSpace").ok()).ok_or(Skip::ColorSpace)?;
    let coding = coding_of(doc, d)?;
    let (width, height) = dims(d).ok_or(Skip::Unreadable)?;
    if space == Space::Cmyk {
        // CMYK only as a JPEG with Adobe's marker, which viewers read to
        // decide how its values are stored. Its /Decode, if any, says the
        // same and is kept.
        let Coding::Jpeg { inflate } = coding else {
            return Err(Skip::ColorSpace);
        };
        let marked = jpeg_bytes(stream, inflate, 64 << 20)
            .and_then(|j| codec::adobe_transform(&j))
            .is_some();
        if !marked {
            return Err(Skip::ColorSpace);
        }
    } else if d.get(b"Decode").is_ok() {
        return Err(Skip::DecodeArray);
    }
    let mask = match d.get(b"SMask") {
        Ok(o) => Some(soft_mask(doc, o, users)?),
        Err(_) => None,
    };
    Ok(Candidate {
        id,
        width,
        height,
        space,
        coding,
        dpi,
        bytes: stream.content.len() as u64,
        mask,
    })
}

/// What a rule did to an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Downsampled,
    Reencoded,
}

/// New data for a soft mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaskData {
    pub id: ObjectId,
    pub width: u32,
    pub height: u32,
    /// Flate.
    pub data: Vec<u8>,
}

/// New data for an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replacement {
    pub id: ObjectId,
    pub width: u32,
    pub height: u32,
    /// `/DCTDecode` when true, `/FlateDecode` otherwise.
    pub jpeg: bool,
    pub data: Vec<u8>,
    pub mask: Option<MaskData>,
    pub change: Change,
    /// Bytes before and after, soft mask included.
    pub before: u64,
    pub after: u64,
}

/// A Flate or lossless photo of at least this many pixels may become a
/// JPEG. Below it, icons and small diagrams stay as they are.
const PHOTO_MIN_PIXELS: u64 = 40_000;
/// ... when the JPEG is under this share of its stored size, per cent.
const PHOTO_MAX_SHARE: u64 = 50;
/// ... and looks at least this alike (luma SSIM). Together these turn a
/// deck's photos stored as PNG into JPEGs and keep its screenshots
/// lossless.
const PHOTO_MIN_SSIM: f64 = 0.95;
/// A JPEG re-encoded at the level's quality is kept only under this share
/// of its stored size, per cent.
const REENCODE_MAX_SHARE: u64 = 80;
/// Images larger than this are left as they are, rather than decoded: an
/// A0 sheet at 300 dpi is 140 million pixels.
pub(crate) const MAX_PIXELS: u64 = 150_000_000;

/// Decodes a sample image (`/FlateDecode` or none) into pixels.
fn decode_samples(stream: &Stream, width: u32, height: u32, space: Space) -> Option<Pixels> {
    let want = u64::from(width) * u64::from(height) * space.channels() as u64;
    if want > MAX_PIXELS * 4 {
        return None;
    }
    let want = want as usize;
    let mut data = if stream.dict.get(b"Filter").is_err() {
        stream.content.clone()
    } else {
        stream.decompressed_content_with_limit(want + 4096).ok()?
    };
    if data.len() < want {
        return None;
    }
    data.truncate(want);
    Some(Pixels {
        width,
        height,
        space,
        data,
    })
}

/// Decodes an image, or its soft mask, as `space`.
fn decode(
    stream: &Stream,
    coding: Coding,
    width: u32,
    height: u32,
    space: Space,
) -> Option<Pixels> {
    let p = match coding {
        Coding::Samples => decode_samples(stream, width, height, space)?,
        Coding::Jpeg { inflate } => {
            let jpeg = jpeg_bytes(stream, inflate, 512 << 20)?;
            codec::decode_jpeg(&jpeg, MAX_PIXELS)?
        }
    };
    (p.width == width && p.height == height && p.space == space).then_some(p)
}

/// Whether `jpeg` is close enough to `p` to stand in for a lossless
/// original.
fn looks_the_same(p: &Pixels, jpeg: &[u8]) -> bool {
    codec::decode_jpeg(jpeg, MAX_PIXELS).is_some_and(|back| {
        codec::ssim(&codec::luma(p), &codec::luma(&back), p.width, p.height) >= PHOTO_MIN_SSIM
    })
}

/// The quality the photo test is made at: `--compress`'s.
const PHOTO_TEST_QUALITY: u8 = 80;

/// Whether a lossless image is a photo, which suits JPEG: its JPEG at
/// `--compress`'s quality looks the same. Asked at that quality on every
/// level, so a level that writes lower quality still makes the same
/// photos JPEGs, rather than keeping more of them lossless and coming out
/// larger than the level below. `jpeg` is `p` at `quality`, reused when
/// that is the test's own quality.
fn is_photo(p: &Pixels, jpeg: &[u8], quality: u8) -> bool {
    if quality == PHOTO_TEST_QUALITY {
        return looks_the_same(p, jpeg);
    }
    codec::encode_jpeg(p, PHOTO_TEST_QUALITY).is_some_and(|j| looks_the_same(p, &j))
}

/// The size an image is resampled to at `level`, or `None` to keep it.
pub(crate) fn target_size(c: &Candidate, level: &Level) -> Option<(u32, u32)> {
    let mut scale: f64 = 1.0;
    if c.dpi > level.above_dpi {
        scale = level.to_dpi / c.dpi;
    }
    if let Some(side) = level.max_side {
        let long = c.width.max(c.height);
        if long > side {
            scale = scale.min(f64::from(side) / f64::from(long));
        }
    }
    if scale >= 1.0 {
        return None;
    }
    let w = ((f64::from(c.width) * scale).round() as u32).max(1);
    let h = ((f64::from(c.height) * scale).round() as u32).max(1);
    (w < c.width || h < c.height).then_some((w, h))
}

/// The rules of `level` for one image: downsample when drawn above the
/// level's threshold, else re-encode a high-quality JPEG, else turn a
/// lossless photo into a JPEG. Each is kept only when it saves what it
/// must; `None` leaves the image as it is.
pub(crate) fn shrink(doc: &Document, c: &Candidate, level: &Level) -> Option<Replacement> {
    let stream = image_stream(doc, c.id)?;
    let mask_bytes = c.mask.as_ref().map_or(0, |m| m.bytes);
    let before = c.bytes + mask_bytes;
    let resizable_mask = c.mask.as_ref().is_none_or(|m| !m.shared);
    let target = target_size(c, level).filter(|_| resizable_mask);
    // Decided before decoding, so an image no rule can touch costs nothing:
    // most of an office scan's, at --compress.
    let requant = match c.coding {
        Coding::Jpeg { inflate } => jpeg_bytes(stream, inflate, 512 << 20)
            .and_then(|j| codec::estimated_quality(&j))
            .is_some_and(|q| q > level.requant_above),
        Coding::Samples => false,
    };
    let photo = c.coding == Coding::Samples
        && c.space != Space::Cmyk
        && u64::from(c.width) * u64::from(c.height) >= PHOTO_MIN_PIXELS;
    if target.is_none() && !requant && !photo {
        return None;
    }
    let pixels = decode(stream, c.coding, c.width, c.height, c.space)?;

    if let Some((w, h)) = target {
        if let Some(r) = downsample(doc, c, &pixels, w, h, level, before) {
            return Some(r);
        }
    }
    if let Coding::Jpeg { .. } = c.coding {
        if requant {
            let data = codec::encode_jpeg(&pixels, level.quality)?;
            if (data.len() as u64) * 100 <= c.bytes * REENCODE_MAX_SHARE {
                return Some(Replacement {
                    id: c.id,
                    width: c.width,
                    height: c.height,
                    jpeg: true,
                    after: data.len() as u64 + mask_bytes,
                    data,
                    mask: None,
                    change: Change::Reencoded,
                    before,
                });
            }
        }
        return None;
    }
    if photo {
        let data = codec::encode_jpeg(&pixels, level.quality)?;
        if (data.len() as u64) * 100 < c.bytes * PHOTO_MAX_SHARE
            && is_photo(&pixels, &data, level.quality)
        {
            return Some(Replacement {
                id: c.id,
                width: c.width,
                height: c.height,
                jpeg: true,
                after: data.len() as u64 + mask_bytes,
                data,
                mask: None,
                change: Change::Reencoded,
                before,
            });
        }
    }
    None
}

fn downsample(
    doc: &Document,
    c: &Candidate,
    pixels: &Pixels,
    w: u32,
    h: u32,
    level: &Level,
    before: u64,
) -> Option<Replacement> {
    let mut small = codec::resize(pixels, w, h)?;
    codec::sharpen_gray(&mut small);
    // A JPEG source stays a JPEG. A lossless one becomes a JPEG only where
    // the photo rule would allow it, so a screenshot drawn large stays
    // sharp.
    let (jpeg, data) = match c.coding {
        Coding::Jpeg { .. } => (true, codec::encode_jpeg(&small, level.quality)?),
        Coding::Samples => {
            let lossless = codec::flate(&small.data);
            match codec::encode_jpeg(&small, level.quality) {
                Some(j) if j.len() < lossless.len() && is_photo(&small, &j, level.quality) => {
                    (true, j)
                }
                _ => (false, lossless),
            }
        }
    };
    let mask = match &c.mask {
        Some(m) => {
            let stream = image_stream(doc, m.id)?;
            let full = decode(stream, m.coding, m.width, m.height, Space::Gray)?;
            let sx = f64::from(w) / f64::from(c.width);
            let sy = f64::from(h) / f64::from(c.height);
            let mw = ((f64::from(m.width) * sx).round() as u32).clamp(1, m.width);
            let mh = ((f64::from(m.height) * sy).round() as u32).clamp(1, m.height);
            let resized = codec::resize(&full, mw, mh)?;
            Some(MaskData {
                id: m.id,
                width: mw,
                height: mh,
                data: codec::flate(&resized.data),
            })
        }
        None => None,
    };
    let after = data.len() as u64 + mask.as_ref().map_or(0, |m| m.data.len() as u64);
    (after < before).then_some(Replacement {
        id: c.id,
        width: w,
        height: h,
        jpeg,
        data,
        mask,
        change: Change::Downsampled,
        before,
        after,
    })
}

/// Pixels decoded at once, across every thread, so a scan of very large
/// pages cannot run the machine out of memory. One image larger than the
/// whole budget still runs, alone.
const PIXELS_IN_FLIGHT: u64 = 64_000_000;
const MAX_THREADS: usize = 8;

struct Budget {
    free: Mutex<u64>,
    freed: Condvar,
}

impl Budget {
    fn take(&self, want: u64) -> u64 {
        let want = want.min(PIXELS_IN_FLIGHT);
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free < want {
            free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= want;
        want
    }

    fn give(&self, n: u64) {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) += n;
        self.freed.notify_all();
    }
}

/// `shrink` over every candidate, on several threads. In candidate order.
pub(crate) fn shrink_all(
    doc: &Document,
    candidates: &[Candidate],
    level: &Level,
) -> Vec<Replacement> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS)
        .min(candidates.len())
        .max(1);
    let next = AtomicUsize::new(0);
    let budget = Budget {
        free: Mutex::new(PIXELS_IN_FLIGHT),
        freed: Condvar::new(),
    };
    let done: Mutex<Vec<(usize, Replacement)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(c) = candidates.get(i) else { break };
                let held = budget.take(u64::from(c.width) * u64::from(c.height));
                let r = shrink(doc, c, level);
                budget.give(held);
                if let Some(r) = r {
                    done.lock().unwrap_or_else(|e| e.into_inner()).push((i, r));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap_or_else(|e| e.into_inner());
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::compress::codec::encode_jpeg;
    use crate::pdf::compress::LEVELS;
    use lopdf::dictionary;

    pub(crate) fn photo(width: u32, height: u32, space: Space) -> Pixels {
        // Photo-like content: gradients, a soft blob, and a little grain,
        // which is what keeps Flate from compressing a real photo well.
        let n = space.channels();
        let mut data = Vec::with_capacity((width * height) as usize * n);
        let mut seed: u32 = 12345;
        for y in 0..height {
            for x in 0..width {
                let fx = f64::from(x) / f64::from(width);
                let fy = f64::from(y) / f64::from(height);
                let blob = (-((fx - 0.5).powi(2) + (fy - 0.4).powi(2)) * 8.0).exp();
                for c in 0..n {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    let grain = f64::from((seed >> 16) % 9) - 4.0;
                    let v = 40.0
                        + 120.0 * fx * (c as f64 * 0.3 + 0.5)
                        + 80.0 * blob
                        + 20.0 * fy
                        + grain;
                    data.push(v.clamp(0.0, 255.0) as u8);
                }
            }
        }
        Pixels {
            width,
            height,
            space,
            data,
        }
    }

    fn screenshot(width: u32, height: u32) -> Pixels {
        // Flat colours and hard one-pixel text strokes.
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                let stroke = (y % 12 < 9) && (x % 7 == 0 || (y % 12 == 4 && x % 3 != 0));
                let v = if stroke { 20 } else { 245 };
                data.extend([v, v, if stroke { 20 } else { 250 }]);
            }
        }
        Pixels {
            width,
            height,
            space: Space::Rgb,
            data,
        }
    }

    fn cs(space: Space) -> &'static str {
        match space {
            Space::Gray => "DeviceGray",
            Space::Rgb => "DeviceRGB",
            Space::Cmyk => "DeviceCMYK",
        }
    }

    fn jpeg_image(doc: &mut Document, p: &Pixels, quality: u8) -> ObjectId {
        let data = encode_jpeg(p, quality).unwrap();
        doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => p.width as i64, "Height" => p.height as i64,
                "ColorSpace" => cs(p.space), "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            data,
        ))
    }

    fn flate_image(doc: &mut Document, p: &Pixels) -> ObjectId {
        doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => p.width as i64, "Height" => p.height as i64,
                "ColorSpace" => cs(p.space), "BitsPerComponent" => 8,
                "Filter" => "FlateDecode",
            },
            codec::flate(&p.data),
        ))
    }

    fn candidate(doc: &Document, id: ObjectId, dpi: f64) -> Candidate {
        let mut placements = Placements::default();
        placements.dpi.insert(id, dpi);
        survey(doc, id, &placements, &mask_users(doc, &[id]))
            .verdict
            .unwrap()
    }

    fn verdict(doc: &Document, id: ObjectId) -> Result<Candidate, Skip> {
        let mut placements = Placements::default();
        placements.dpi.insert(id, 300.0);
        survey(doc, id, &placements, &mask_users(doc, &[id])).verdict
    }

    fn l1() -> &'static Level {
        &LEVELS[1]
    }

    #[test]
    fn a_300_dpi_jpeg_is_downsampled_to_150() {
        let mut doc = Document::with_version("1.7");
        let id = jpeg_image(&mut doc, &photo(600, 400, Space::Rgb), 85);
        let c = candidate(&doc, id, 300.0);
        let r = shrink(&doc, &c, l1()).unwrap();
        assert_eq!(
            (r.width, r.height, r.jpeg, r.change),
            (300, 200, true, Change::Downsampled)
        );
        assert!(r.after < r.before);
    }

    #[test]
    fn an_image_at_or_under_the_threshold_keeps_its_size() {
        let mut doc = Document::with_version("1.7");
        let id = jpeg_image(&mut doc, &photo(400, 300, Space::Gray), 75);
        assert_eq!(target_size(&candidate(&doc, id, 225.0), l1()), None);
        assert_eq!(shrink(&doc, &candidate(&doc, id, 200.0), l1()), None);
        assert_eq!(
            target_size(&candidate(&doc, id, 226.0), l1()),
            Some((265, 199))
        );
    }

    #[test]
    fn a_high_quality_jpeg_is_reencoded_at_the_same_size() {
        let mut doc = Document::with_version("1.7");
        let id = jpeg_image(&mut doc, &photo(400, 300, Space::Rgb), 98);
        let r = shrink(&doc, &candidate(&doc, id, 150.0), l1()).unwrap();
        assert_eq!((r.width, r.height, r.change), (400, 300, Change::Reencoded));
        assert!(r.after * 100 <= r.before * 80);
        // One already at the level's quality is left alone.
        let id = jpeg_image(&mut doc, &photo(400, 300, Space::Rgb), 80);
        assert_eq!(shrink(&doc, &candidate(&doc, id, 150.0), l1()), None);
    }

    #[test]
    fn a_lossless_photo_becomes_a_jpeg_and_a_screenshot_stays_lossless() {
        let mut doc = Document::with_version("1.7");
        let p = flate_image(&mut doc, &photo(400, 300, Space::Rgb));
        let r = shrink(&doc, &candidate(&doc, p, 150.0), l1()).unwrap();
        assert_eq!((r.jpeg, r.change), (true, Change::Reencoded));
        let s = flate_image(&mut doc, &screenshot(400, 300));
        assert_eq!(shrink(&doc, &candidate(&doc, s, 150.0), l1()), None);
        // Drawn small, a screenshot is downsampled.
        let r = shrink(&doc, &candidate(&doc, s, 800.0), l1()).unwrap();
        assert_eq!((r.change, r.width), (Change::Downsampled, 75));
        // A small lossless image is never made a JPEG.
        let tiny = flate_image(&mut doc, &photo(150, 150, Space::Rgb));
        assert_eq!(shrink(&doc, &candidate(&doc, tiny, 150.0), l1()), None);
    }

    #[test]
    fn a_harder_level_makes_the_same_photos_jpegs() {
        let mut doc = Document::with_version("1.7");
        let p = flate_image(&mut doc, &photo(400, 300, Space::Rgb));
        for level in &LEVELS[1..] {
            let r = shrink(&doc, &candidate(&doc, p, 100.0), level).unwrap();
            assert!(r.jpeg, "L{}", level.n);
        }
    }

    #[test]
    fn a_soft_mask_is_resized_with_its_image_unless_shared() {
        let mut doc = Document::with_version("1.7");
        let mask = Pixels {
            width: 600,
            height: 400,
            space: Space::Gray,
            data: photo(600, 400, Space::Gray).data,
        };
        let mask_id = flate_image(&mut doc, &mask);
        let id = jpeg_image(&mut doc, &photo(600, 400, Space::Rgb), 85);
        doc.get_object_mut(id)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set("SMask", mask_id);
        let c = candidate(&doc, id, 300.0);
        assert!(c.mask.as_ref().is_some_and(|m| !m.shared));
        let r = shrink(&doc, &c, l1()).unwrap();
        let m = r.mask.unwrap();
        assert_eq!((m.id, m.width, m.height), (mask_id, 300, 200));

        // A second image using the same mask: neither is downsampled.
        let other = jpeg_image(&mut doc, &photo(600, 400, Space::Rgb), 85);
        doc.get_object_mut(other)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set("SMask", mask_id);
        let mut placements = Placements::default();
        placements.dpi.insert(id, 300.0);
        let users = mask_users(&doc, &[id, other]);
        let c = survey(&doc, id, &placements, &users).verdict.unwrap();
        assert!(c.mask.as_ref().unwrap().shared);
        assert_eq!(shrink(&doc, &c, l1()), None);
    }

    #[test]
    fn ineligible_images_say_why() {
        let mut doc = Document::with_version("1.7");
        let base = || {
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 10, "Height" => 10,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
            }
        };
        let mut add = |f: &dyn Fn(&mut Dictionary)| {
            let mut d = base();
            f(&mut d);
            doc.add_object(Stream::new(d, vec![0; 100]))
        };
        let cases: Vec<(ObjectId, Skip)> = vec![
            (add(&|d| d.set("ImageMask", true)), Skip::OneBit),
            (add(&|d| d.set("BitsPerComponent", 1)), Skip::OneBit),
            (add(&|d| d.set("Filter", "CCITTFaxDecode")), Skip::OneBit),
            (add(&|d| d.set("Filter", "JPXDecode")), Skip::Jpeg2000),
            (add(&|d| d.set("BitsPerComponent", 16)), Skip::BitDepth),
            (add(&|d| d.set("Filter", "LZWDecode")), Skip::Filter),
            (
                add(&|d| {
                    d.set("Filter", "DCTDecode");
                    d.set("DecodeParms", dictionary! { "ColorTransform" => 0 });
                }),
                Skip::Filter,
            ),
            (add(&|d| d.set("ColorSpace", "Indexed")), Skip::ColorSpace),
            (
                add(&|d| d.set("ColorSpace", "DeviceCMYK")),
                Skip::ColorSpace,
            ),
            (
                add(&|d| d.set("Decode", vec![1.into(), 0.into()])),
                Skip::DecodeArray,
            ),
            (
                add(&|d| d.set("Mask", vec![0.into(), 10.into()])),
                Skip::Mask,
            ),
        ];
        for (id, why) in cases {
            assert_eq!(verdict(&doc, id), Err(why), "{id:?}");
        }
        // Drawn nowhere, or on a page conv could not read.
        let id = flate_image(&mut doc, &photo(10, 10, Space::Gray));
        let placements = Placements::default();
        let users = HashMap::new();
        assert_eq!(
            survey(&doc, id, &placements, &users).verdict,
            Err(Skip::NotDrawn)
        );
        let mut placements = Placements::default();
        placements.unknown.insert(id);
        assert_eq!(
            survey(&doc, id, &placements, &users).verdict,
            Err(Skip::Unreadable)
        );
    }

    #[test]
    fn icc_profiles_give_the_component_count() {
        let mut doc = Document::with_version("1.7");
        let profile = doc.add_object(Stream::new(dictionary! { "N" => 3 }, vec![]));
        let id = flate_image(&mut doc, &photo(10, 10, Space::Rgb));
        doc.get_object_mut(id)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set(
                "ColorSpace",
                vec![Object::Name(b"ICCBased".to_vec()), profile.into()],
            );
        assert_eq!(verdict(&doc, id).unwrap().space, Space::Rgb);
    }

    #[test]
    fn cmyk_is_eligible_only_as_a_jpeg_with_adobes_marker() {
        let mut doc = Document::with_version("1.7");
        let id = jpeg_image(&mut doc, &photo(600, 400, Space::Cmyk), 90);
        doc.get_object_mut(id)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set(
                "Decode",
                vec![
                    1.into(),
                    0.into(),
                    1.into(),
                    0.into(),
                    1.into(),
                    0.into(),
                    1.into(),
                    0.into(),
                ],
            );
        let c = candidate(&doc, id, 400.0);
        assert_eq!(c.space, Space::Cmyk);
        let r = shrink(&doc, &c, l1()).unwrap();
        assert_eq!((r.width, r.height), (225, 150));
        assert_eq!(codec::adobe_transform(&r.data), Some(2));
        // A CMYK JPEG without the marker is left alone.
        let p = photo(40, 40, Space::Cmyk);
        let mut bare = encode_jpeg(&p, 90).unwrap();
        let at = bare
            .windows(9)
            .position(|w| w == b"\xFF\xEE\x00\x0EAdobe")
            .unwrap();
        bare.drain(at..at + 16);
        assert_eq!(codec::adobe_transform(&bare), None);
        let bare_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 40, "Height" => 40,
                "ColorSpace" => "DeviceCMYK", "BitsPerComponent" => 8, "Filter" => "DCTDecode",
            },
            bare,
        ));
        assert_eq!(verdict(&doc, bare_id), Err(Skip::ColorSpace));
    }

    #[test]
    fn a_flate_wrapped_jpeg_is_read_and_loses_the_wrapper() {
        let mut doc = Document::with_version("1.7");
        let jpeg = encode_jpeg(&photo(600, 400, Space::Gray), 85).unwrap();
        let id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 600, "Height" => 400,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
                "Filter" => vec![Object::Name(b"FlateDecode".to_vec()), Object::Name(b"DCTDecode".to_vec())],
            },
            codec::flate(&jpeg),
        ));
        let c = candidate(&doc, id, 300.0);
        assert_eq!(c.coding, Coding::Jpeg { inflate: true });
        let r = shrink(&doc, &c, l1()).unwrap();
        assert!(r.jpeg && r.data.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn many_images_are_shrunk_in_order() {
        let mut doc = Document::with_version("1.7");
        let ids: Vec<ObjectId> = (0..12)
            .map(|i| jpeg_image(&mut doc, &photo(300 + i, 200, Space::Rgb), 90))
            .collect();
        let candidates: Vec<Candidate> = ids.iter().map(|&id| candidate(&doc, id, 300.0)).collect();
        let done = shrink_all(&doc, &candidates, l1());
        assert_eq!(done.iter().map(|r| r.id).collect::<Vec<_>>(), ids);
    }

    #[test]
    fn a_pixel_cap_shrinks_the_long_side() {
        let mut doc = Document::with_version("1.7");
        let id = jpeg_image(&mut doc, &photo(400, 100, Space::Gray), 80);
        let c = candidate(&doc, id, 100.0);
        let capped = Level {
            max_side: Some(200),
            ..LEVELS[1]
        };
        assert_eq!(target_size(&c, &capped), Some((200, 50)));
        assert_eq!(target_size(&c, l1()), None);
    }
}
