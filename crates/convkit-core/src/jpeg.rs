//! A JPEG's location and metadata, read and removed as plain bytes.
//!
//! Typst embeds a JPEG byte for byte, EXIF and GPS included, so conv checks
//! for a recorded location itself and, for `--strip-metadata`, drops the
//! metadata segments without touching a single pixel: nothing is decoded or
//! re-encoded. What stays is what changes how the picture looks: JFIF, the
//! ICC profile, Adobe's colour flag, and a minimal EXIF holding only the
//! orientation and resolution.

use std::path::Path;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP14: u8 = 0xEE;
const COM: u8 = 0xFE;

const EXIF_HEADER: &[u8] = b"Exif\0\0";

const TAG_ORIENTATION: u16 = 0x0112;
const TAG_X_RESOLUTION: u16 = 0x011A;
const TAG_Y_RESOLUTION: u16 = 0x011B;
const TAG_RESOLUTION_UNIT: u16 = 0x0128;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_GPS_LATITUDE: u16 = 0x0002;

const TYPE_SHORT: u16 = 3;
const TYPE_RATIONAL: u16 = 5;

/// One marker segment: the bytes from its `FF xx` to the start of the next
/// one. For SOS that includes the entropy-coded data after the header.
struct Segment<'a> {
    marker: u8,
    /// Everything the segment occupies in the file.
    raw: &'a [u8],
    /// The bytes after the length field (empty for markers without one).
    payload: &'a [u8],
}

/// The segments from SOI up to and including the first EOI.
fn segments(bytes: &[u8]) -> Result<Vec<Segment<'_>>, String> {
    let mut out = Vec::new();
    walk(bytes, false, &mut out)?;
    Ok(out)
}

/// The segments before the image data, as far as the file parses: whatever
/// was read before damage is still returned. Metadata sits in front of the
/// first scan, so damage after it does not change what the file records.
fn header_segments(bytes: &[u8]) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let _ = walk(bytes, true, &mut out);
    out
}

/// Reads segments into `out` until the first EOI (or, with `header_only`,
/// the first SOS), keeping what it read when it hits an error.
fn walk<'a>(bytes: &'a [u8], header_only: bool, out: &mut Vec<Segment<'a>>) -> Result<(), String> {
    if !bytes.starts_with(&[0xFF, SOI]) {
        return Err("not a JPEG".into());
    }
    let truncated = || "ends before the image data".to_string();
    out.push(Segment {
        marker: SOI,
        raw: &bytes[..2],
        payload: &[],
    });
    let mut pos = 2;
    loop {
        // A marker is FF, any number of FF fill bytes, then its code.
        if *bytes.get(pos).ok_or_else(truncated)? != 0xFF {
            return Err("not a JPEG".into());
        }
        let mut code_at = pos + 1;
        while bytes.get(code_at) == Some(&0xFF) {
            code_at += 1;
        }
        let marker = *bytes.get(code_at).ok_or_else(truncated)?;
        let start = pos;
        let header_end = code_at + 1;
        let lengthless = marker == 0x01 || marker == 0x00 || (0xD0..=0xD9).contains(&marker);
        if lengthless {
            out.push(Segment {
                marker,
                raw: &bytes[start..header_end],
                payload: &[],
            });
            if marker == EOI {
                return Ok(());
            }
            pos = header_end;
            continue;
        }
        let len = bytes
            .get(header_end..header_end + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .ok_or_else(truncated)?;
        if len < 2 {
            return Err("not a JPEG".into());
        }
        let payload_end = header_end + len;
        if payload_end > bytes.len() {
            return Err(truncated());
        }
        let mut end = payload_end;
        if marker == SOS && header_only {
            return Ok(());
        }
        if marker == SOS {
            end = scan_end(bytes, payload_end).ok_or_else(truncated)?;
        }
        out.push(Segment {
            marker,
            raw: &bytes[start..end],
            payload: &bytes[header_end + 2..payload_end],
        });
        pos = end;
    }
}

/// Where the entropy-coded data starting at `from` ends: at the first marker
/// that is not a stuffed `FF00` or a restart (`FFD0`-`FFD7`).
fn scan_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let mut code_at = i + 1;
        while bytes.get(code_at) == Some(&0xFF) {
            code_at += 1;
        }
        let code = *bytes.get(code_at)?;
        if code == 0x00 || (0xD0..=0xD7).contains(&code) {
            i = code_at + 1;
        } else {
            return Some(i);
        }
    }
    None
}

/// A TIFF structure (the EXIF payload) read with bounds checks everywhere.
struct Tiff<'a> {
    data: &'a [u8],
    little: bool,
}

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    /// Where the four-byte value (or offset) field sits.
    value_at: usize,
}

impl<'a> Tiff<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        let little = match data.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let tiff = Tiff { data, little };
        (tiff.u16(2)? == 42).then_some(tiff)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn ifd0(&self) -> Option<Vec<Entry>> {
        self.entries(usize::try_from(self.u32(4)?).ok()?)
    }

    fn entries(&self, at: usize) -> Option<Vec<Entry>> {
        let n = usize::from(self.u16(at)?);
        let first = at.checked_add(2)?;
        // The whole table must fit, or the IFD is malformed.
        self.data.get(first..first.checked_add(n * 12)?)?;
        (0..n)
            .map(|i| {
                let e = first + i * 12;
                Some(Entry {
                    tag: self.u16(e)?,
                    kind: self.u16(e + 2)?,
                    count: self.u32(e + 4)?,
                    value_at: e + 8,
                })
            })
            .collect()
    }

    /// A SHORT stored inline, if the entry is exactly one.
    fn short(&self, e: &Entry) -> Option<u16> {
        (e.kind == TYPE_SHORT && e.count == 1)
            .then(|| self.u16(e.value_at))
            .flatten()
    }

    /// A RATIONAL's numerator and denominator, if the entry is exactly one.
    fn rational(&self, e: &Entry) -> Option<(u32, u32)> {
        if e.kind != TYPE_RATIONAL || e.count != 1 {
            return None;
        }
        let at = usize::try_from(self.u32(e.value_at)?).ok()?;
        Some((self.u32(at)?, self.u32(at.checked_add(4)?)?))
    }
}

fn exif_tiff<'a>(segs: &[Segment<'a>]) -> Option<Tiff<'a>> {
    segs.iter()
        .filter(|s| s.marker == APP1)
        .find_map(|s| Tiff::new(s.payload.strip_prefix(EXIF_HEADER)?))
}

#[allow(dead_code)]
/// Whether the JPEG records where it was taken: an EXIF GPS block with a
/// latitude.
pub(crate) fn has_location(bytes: &[u8]) -> bool {
    let segs = header_segments(bytes);
    let Some(tiff) = exif_tiff(&segs) else {
        return false;
    };
    let Some(ifd0) = tiff.ifd0() else {
        return false;
    };
    ifd0.iter()
        .filter(|e| e.tag == TAG_GPS_IFD)
        .filter_map(|e| usize::try_from(tiff.u32(e.value_at)?).ok())
        .filter_map(|at| tiff.entries(at))
        .any(|gps| gps.iter().any(|e| e.tag == TAG_GPS_LATITUDE))
}

#[allow(dead_code)]
/// `has_location` for a file; false when it cannot be read.
pub(crate) fn file_has_location(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| has_location(&b))
}

/// The JPEG without its metadata, pixels untouched. Fails with a short
/// reason when the bytes are not a whole JPEG.
pub(crate) fn strip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let segs = segments(bytes)?;
    let exif = minimal_exif(&segs);
    let mut out = Vec::with_capacity(bytes.len());
    let mut exif_pending = exif.is_some();
    for seg in &segs {
        if !keep(seg) {
            continue;
        }
        // The EXIF goes after SOI and any APP0, before everything else.
        if exif_pending && seg.marker != SOI && seg.marker != APP0 {
            out.extend_from_slice(exif.as_deref().unwrap_or_default());
            exif_pending = false;
        }
        out.extend_from_slice(seg.raw);
    }
    Ok(out)
}

fn keep(seg: &Segment) -> bool {
    match seg.marker {
        APP0 => true,
        APP2 => seg.payload.starts_with(b"ICC_PROFILE\0"),
        APP14 => seg.payload.starts_with(b"Adobe"),
        COM | 0xE1..=0xEF => false,
        _ => true,
    }
}

/// A whole APP1 segment holding only the orientation (when it is not the
/// default) and the resolution tags the source had; none if there are none.
fn minimal_exif(segs: &[Segment]) -> Option<Vec<u8>> {
    enum Value {
        Short(u16),
        Rational(u32, u32),
    }

    let tiff = exif_tiff(segs)?;
    let ifd0 = tiff.ifd0()?;
    let find = |tag| ifd0.iter().find(|e| e.tag == tag);

    let mut tags: Vec<(u16, Value)> = Vec::new();
    if let Some(o) = find(TAG_ORIENTATION)
        .and_then(|e| tiff.short(e))
        .filter(|&o| o != 1)
    {
        tags.push((TAG_ORIENTATION, Value::Short(o)));
    }
    for tag in [TAG_X_RESOLUTION, TAG_Y_RESOLUTION] {
        if let Some((n, d)) = find(tag).and_then(|e| tiff.rational(e)) {
            tags.push((tag, Value::Rational(n, d)));
        }
    }
    if let Some(u) = find(TAG_RESOLUTION_UNIT).and_then(|e| tiff.short(e)) {
        tags.push((TAG_RESOLUTION_UNIT, Value::Short(u)));
    }
    if tags.is_empty() {
        return None;
    }

    // Little-endian TIFF: header, IFD0, then the rationals it points to.
    let mut table = Vec::new();
    let mut data = Vec::new();
    let data_start = 8 + 2 + tags.len() * 12 + 4;
    for (tag, value) in &tags {
        table.extend_from_slice(&tag.to_le_bytes());
        match value {
            Value::Short(v) => {
                table.extend_from_slice(&TYPE_SHORT.to_le_bytes());
                table.extend_from_slice(&1u32.to_le_bytes());
                table.extend_from_slice(&v.to_le_bytes());
                table.extend_from_slice(&[0, 0]);
            }
            Value::Rational(n, d) => {
                table.extend_from_slice(&TYPE_RATIONAL.to_le_bytes());
                table.extend_from_slice(&1u32.to_le_bytes());
                table.extend_from_slice(&((data_start + data.len()) as u32).to_le_bytes());
                data.extend_from_slice(&n.to_le_bytes());
                data.extend_from_slice(&d.to_le_bytes());
            }
        }
    }

    let mut payload = Vec::from(EXIF_HEADER);
    payload.extend_from_slice(b"II*\0");
    payload.extend_from_slice(&8u32.to_le_bytes());
    payload.extend_from_slice(&(tags.len() as u16).to_le_bytes());
    payload.extend_from_slice(&table);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&data);

    let len = u16::try_from(payload.len() + 2).ok()?;
    let mut seg = vec![0xFF, APP1];
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(&payload);
    Some(seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// An APP1 Exif payload. Entries are (tag, type, count, value): SHORT
    /// and LONG values sit inline; a RATIONAL entry writes `count`
    /// rationals of value/1; an ASCII entry writes `count` bytes of
    /// "Apple". A GPS IFD pointer's value is filled in.
    fn exif(
        le: bool,
        ifd0: &[(u16, u16, u32, u32)],
        gps: Option<&[(u16, u16, u32, u32)]>,
    ) -> Vec<u8> {
        let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let ifd_len = |n: usize| 2 + n * 12 + 4;
        let gps_at = 8 + ifd_len(ifd0.len());
        let mut data_at = gps_at + gps.map_or(0, |g| ifd_len(g.len()));
        let mut data = Vec::new();
        let mut table = |entries: &[(u16, u16, u32, u32)]| {
            let mut out = Vec::new();
            out.extend_from_slice(&w16(entries.len() as u16));
            for &(tag, kind, count, value) in entries {
                out.extend_from_slice(&w16(tag));
                out.extend_from_slice(&w16(kind));
                out.extend_from_slice(&w32(count));
                match kind {
                    3 => {
                        out.extend_from_slice(&w16(value as u16));
                        out.extend_from_slice(&[0, 0]);
                    }
                    5 | 2 => {
                        out.extend_from_slice(&w32(data_at as u32));
                        let before = data.len();
                        if kind == 5 {
                            for _ in 0..count {
                                data.extend_from_slice(&w32(value));
                                data.extend_from_slice(&w32(1));
                            }
                        } else {
                            let mut s = b"Apple\0".repeat(count as usize);
                            s.truncate(count as usize);
                            data.extend_from_slice(&s);
                        }
                        data_at += data.len() - before;
                    }
                    _ if tag == TAG_GPS_IFD => out.extend_from_slice(&w32(gps_at as u32)),
                    _ => out.extend_from_slice(&w32(value)),
                }
            }
            out.extend_from_slice(&w32(0));
            out
        };
        let t0 = table(ifd0);
        let tg = gps.map(&mut table).unwrap_or_default();
        let mut tiff = Vec::from(if le { *b"II" } else { *b"MM" });
        tiff.extend_from_slice(&w16(42));
        tiff.extend_from_slice(&w32(8));
        tiff.extend_from_slice(&t0);
        tiff.extend_from_slice(&tg);
        tiff.extend_from_slice(&data);
        let mut payload = Vec::from(EXIF_HEADER);
        payload.extend_from_slice(&tiff);
        payload
    }

    fn jpeg(parts: &[Vec<u8>], scan: &[u8], trailer: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, SOI];
        for p in parts {
            v.extend_from_slice(p);
        }
        v.extend_from_slice(&segment(0xC0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]));
        v.extend_from_slice(&segment(SOS, &[1, 1, 0, 0, 63, 0]));
        v.extend_from_slice(scan);
        v.extend_from_slice(&[0xFF, EOI]);
        v.extend_from_slice(trailer);
        v
    }

    const SCAN: &[u8] = &[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD3, 0x56];

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    const GPS_POINTER: (u16, u16, u32, u32) = (TAG_GPS_IFD, 4, 1, 0);
    const LATITUDE: (u16, u16, u32, u32) = (TAG_GPS_LATITUDE, 5, 3, 40);

    #[test]
    fn location_is_read_from_a_gps_latitude() {
        for le in [true, false] {
            let e = exif(le, &[GPS_POINTER], Some(&[LATITUDE]));
            let j = jpeg(&[segment(APP1, &e)], SCAN, &[]);
            assert!(has_location(&j), "little-endian: {le}");
        }
    }

    #[test]
    fn no_gps_pointer_or_no_latitude_is_no_location() {
        for le in [true, false] {
            let plain = exif(le, &[(TAG_ORIENTATION, 3, 1, 6)], None);
            assert!(!has_location(&jpeg(&[segment(APP1, &plain)], SCAN, &[])));
            let no_lat = exif(le, &[GPS_POINTER], Some(&[(0x0001, 3, 1, 78)]));
            assert!(!has_location(&jpeg(&[segment(APP1, &no_lat)], SCAN, &[])));
        }
        assert!(!has_location(&jpeg(&[], SCAN, &[])));
    }

    #[test]
    fn garbage_and_truncated_input_is_no_location() {
        assert!(!has_location(b""));
        assert!(!has_location(b"\xFF\xD8"));
        assert!(!has_location(b"random bytes that are not a jpeg"));
        let e = exif(true, &[GPS_POINTER], Some(&[LATITUDE]));
        let j = jpeg(&[segment(APP1, &e)], SCAN, &[]);
        for cut in 0..j.len() {
            let _ = has_location(&j[..cut]);
        }
        // The GPS pointer aims far outside the data.
        let mut bad = exif(true, &[GPS_POINTER], Some(&[LATITUDE]));
        bad[6 + 8 + 2 + 8..6 + 8 + 2 + 12].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(!has_location(&jpeg(&[segment(APP1, &bad)], SCAN, &[])));
    }

    #[test]
    fn location_survives_damage_after_the_exif() {
        let e = exif(true, &[GPS_POINTER], Some(&[LATITUDE]));
        let exif_seg = segment(APP1, &e);
        let j = jpeg(std::slice::from_ref(&exif_seg), SCAN, &[]);
        // No EOI, or cut anywhere inside the scan.
        for cut in j.len() - SCAN.len() - 2..j.len() {
            assert!(has_location(&j[..cut]), "cut at {cut}");
        }
        assert!(has_location(&j[..j.len() / 2 + exif_seg.len() / 2]));
        // Junk between two header segments, after the Exif.
        let junk = jpeg(
            &[exif_seg, vec![0x00, 0x00], segment(0xDB, &[0; 65])],
            SCAN,
            &[],
        );
        assert!(has_location(&junk));
    }

    #[test]
    fn strip_drops_exif_xmp_iptc_mpf_and_comments() {
        let e = exif(true, &[(0x010F, 2, 6, 0), GPS_POINTER], Some(&[LATITUDE]));
        let j = jpeg(
            &[
                segment(APP1, &e),
                segment(APP1, b"http://ns.adobe.com/xap/1.0/\0<x/>"),
                segment(0xED, b"Photoshop 3.0\0iptc"),
                segment(APP2, b"MPF\0mpfdata"),
                segment(COM, b"a comment"),
            ],
            SCAN,
            &[],
        );
        let out = strip(&j).unwrap();
        assert!(!contains(&out, b"Exif"));
        assert!(!contains(&out, b"http://ns.adobe.com/xap/1.0/"));
        assert!(!contains(&out, b"Photoshop"));
        assert!(!contains(&out, b"MPF\0"));
        assert!(!contains(&out, &[0xFF, COM]));
        assert!(!contains(&out, b"Apple"));
        assert!(!has_location(&out));
    }

    #[test]
    fn strip_keeps_jfif_icc_adobe_and_the_scan_bytes() {
        let jfif = segment(APP0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let icc = segment(APP2, b"ICC_PROFILE\0\x01\x01profile");
        let adobe = segment(APP14, b"Adobe\0\x64\0\0\0\0\x01");
        let j = jpeg(&[jfif.clone(), icc.clone(), adobe.clone()], SCAN, &[]);
        let out = strip(&j).unwrap();
        assert!(contains(&out, &jfif));
        assert!(contains(&out, &icc));
        assert!(contains(&out, &adobe));
        assert!(contains(&out, SCAN));
        assert_eq!(out, j);
    }

    #[test]
    fn strip_keeps_orientation_and_resolution_only() {
        let e = exif(
            true,
            &[
                (0x010F, 2, 6, 0),
                (TAG_ORIENTATION, 3, 1, 6),
                (TAG_X_RESOLUTION, 5, 1, 300),
                (TAG_Y_RESOLUTION, 5, 1, 300),
                (TAG_RESOLUTION_UNIT, 3, 1, 2),
                GPS_POINTER,
            ],
            Some(&[LATITUDE]),
        );
        let jfif = segment(APP0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let out = strip(&jpeg(&[jfif.clone(), segment(APP1, &e)], SCAN, &[])).unwrap();
        assert!(!has_location(&out));
        assert!(!contains(&out, b"Apple"));
        // JFIF first, then the minimal EXIF.
        assert_eq!(&out[2..2 + jfif.len()], &jfif[..]);
        let segs = segments(&out).unwrap();
        assert_eq!(segs[2].marker, APP1);
        let tiff = Tiff::new(segs[2].payload.strip_prefix(EXIF_HEADER).unwrap()).unwrap();
        let ifd0 = tiff.ifd0().unwrap();
        let tags: Vec<u16> = ifd0.iter().map(|e| e.tag).collect();
        assert_eq!(
            tags,
            [
                TAG_ORIENTATION,
                TAG_X_RESOLUTION,
                TAG_Y_RESOLUTION,
                TAG_RESOLUTION_UNIT
            ]
        );
        assert_eq!(tiff.short(&ifd0[0]), Some(6));
        assert_eq!(tiff.rational(&ifd0[1]), Some((300, 1)));
        assert_eq!(tiff.rational(&ifd0[2]), Some((300, 1)));
        assert_eq!(tiff.short(&ifd0[3]), Some(2));
    }

    #[test]
    fn strip_keeps_orientation_from_a_big_endian_source() {
        let e = exif(false, &[(TAG_ORIENTATION, 3, 1, 8)], None);
        let out = strip(&jpeg(&[segment(APP1, &e)], SCAN, &[])).unwrap();
        let segs = segments(&out).unwrap();
        let tiff = Tiff::new(segs[1].payload.strip_prefix(EXIF_HEADER).unwrap()).unwrap();
        assert_eq!(tiff.short(&tiff.ifd0().unwrap()[0]), Some(8));
    }

    #[test]
    fn strip_writes_no_exif_when_nothing_is_worth_keeping() {
        let e = exif(
            true,
            &[(TAG_ORIENTATION, 3, 1, 1), GPS_POINTER],
            Some(&[LATITUDE]),
        );
        let j = jpeg(&[segment(APP1, &e)], SCAN, &[]);
        let out = strip(&j).unwrap();
        assert_eq!(out, jpeg(&[], SCAN, &[]));
    }

    #[test]
    fn strip_stops_at_the_first_eoi() {
        let j = jpeg(&[], SCAN, b"MPF trailing image bytes");
        let out = strip(&j).unwrap();
        assert!(!contains(&out, b"trailing"));
        assert!(out.ends_with(&[0xFF, EOI]));
    }

    #[test]
    fn strip_keeps_every_scan_of_a_progressive_file() {
        let mut j = vec![0xFF, SOI];
        let parts = [
            segment(0xC2, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]),
            segment(SOS, &[1, 1, 0, 0, 0, 1]),
            vec![0x01, 0xFF, 0x00, 0xFF, 0xFF],
            segment(
                0xC4,
                &[0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5],
            ),
            segment(SOS, &[1, 1, 0, 1, 63, 0]),
            vec![0x02, 0xFF, 0xD0, 0x03],
        ];
        for p in &parts {
            j.extend_from_slice(p);
        }
        j.extend_from_slice(&[0xFF, EOI]);
        assert_eq!(strip(&j).unwrap(), j);
    }

    #[test]
    fn strip_refuses_what_is_not_a_jpeg() {
        assert_eq!(strip(b"\x89PNG\r\n\x1a\n").unwrap_err(), "not a JPEG");
        assert_eq!(strip(b"").unwrap_err(), "not a JPEG");
    }

    #[test]
    fn strip_refuses_a_truncated_jpeg_without_panicking() {
        let j = jpeg(&[segment(APP0, b"JFIF\0")], SCAN, &[]);
        for cut in 2..j.len() - 2 {
            assert!(strip(&j[..cut]).is_err(), "cut at {cut}");
        }
    }
}
