//! A JPEG's orientation tag, read and reset in place. A plain file read, no
//! backend.
//!
//! Only the headers before the picture are read: the EXIF tag (0x0112 in
//! the first IFD of the `Exif` APP1 segment), and any copy of it in an XMP
//! packet, which some editors write too. Resetting writes the same number
//! of bytes in the same places, so nothing else in the file moves.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::rotate::Orientation;

const EXIF: &[u8] = b"Exif\0\0";
const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const ORIENTATION: u16 = 0x0112;
const SHORT: u16 = 3;

/// The orientation a JPEG's EXIF tag gives it. `None` when the file is not
/// a JPEG, has no tag, or has one of a kind no writer uses.
pub(crate) fn read(path: &Path) -> Option<Orientation> {
    let mut r = BufReader::new(File::open(path).ok()?);
    let tags = locate(&mut r).ok()??;
    Orientation::from_exif(tags.value)
}

/// Rewrites the tag, and any XMP copy of it, to 1: shown as stored.
pub(crate) fn set_upright(path: &Path) -> io::Result<()> {
    let mut f = File::options().read(true).write(true).open(path)?;
    let tags = locate(&mut BufReader::new(&f))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "it has no orientation tag"))?;
    let one = if tags.big_endian { [0, 1] } else { [1, 0] };
    f.seek(SeekFrom::Start(tags.exif_at))?;
    f.write_all(&one)?;
    for at in tags.xmp_at {
        f.seek(SeekFrom::Start(at))?;
        f.write_all(b"1")?;
    }
    f.flush()
}

/// Where the orientation is written, and what it says.
struct Tags {
    value: u16,
    /// The file offset of the tag's two-byte value.
    exif_at: u64,
    big_endian: bool,
    /// The file offsets of the digit in each XMP copy.
    xmp_at: Vec<u64>,
}

/// Walks the segments ahead of the picture, `SOI` to `SOS`, for the tag.
fn locate<R: Read + Seek>(r: &mut R) -> io::Result<Option<Tags>> {
    let mut soi = [0; 2];
    r.read_exact(&mut soi)?;
    if soi != [0xFF, 0xD8] {
        return Ok(None);
    }
    let mut found: Option<Tags> = None;
    let mut xmp_at = Vec::new();
    loop {
        let mut byte = [0; 1];
        r.read_exact(&mut byte)?;
        if byte[0] != 0xFF {
            return Ok(None);
        }
        // Any number of 0xFF may pad a marker.
        let mut marker = 0xFF;
        while marker == 0xFF {
            r.read_exact(&mut byte)?;
            marker = byte[0];
        }
        match marker {
            // The scan or the end: no header follows.
            0xDA | 0xD9 => break,
            // Markers that stand alone, with no length.
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        let mut len = [0; 2];
        r.read_exact(&mut len)?;
        let len = u64::from(u16::from_be_bytes(len));
        if len < 2 {
            return Ok(None);
        }
        let start = r.stream_position()?;
        if marker == 0xE1 {
            let mut data = vec![0; (len - 2) as usize];
            r.read_exact(&mut data)?;
            if let Some(tiff) = data.strip_prefix(EXIF) {
                if found.is_none() {
                    found = in_tiff(tiff, start + EXIF.len() as u64);
                }
            } else if let Some(packet) = data.strip_prefix(XMP) {
                let base = start + XMP.len() as u64;
                xmp_at.extend(xmp_digits(packet).into_iter().map(|i| base + i as u64));
            }
        } else {
            r.seek(SeekFrom::Start(start + len - 2))?;
        }
    }
    Ok(found.map(|t| Tags { xmp_at, ..t }))
}

/// The orientation entry of a TIFF block's first IFD, which starts at file
/// offset `base`. A SHORT holding one value, as the EXIF standard has it.
fn in_tiff(tiff: &[u8], base: u64) -> Option<Tags> {
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |i: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(i..i + 2)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(i..i + 4)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?);
    (0..count)
        .map(|i| ifd + 2 + 12 * i)
        .find(|&entry| u16_at(entry) == Some(ORIENTATION))
        .filter(|&entry| u16_at(entry + 2) == Some(SHORT) && u32_at(entry + 4) == Some(1))
        .and_then(|entry| {
            Some(Tags {
                value: u16_at(entry + 8)?,
                exif_at: base + (entry + 8) as u64,
                big_endian,
                xmp_at: Vec::new(),
            })
        })
}

/// The positions of the digit in each `tiff:Orientation` of an XMP packet,
/// written as an attribute (`tiff:Orientation="6"`) or an element
/// (`<tiff:Orientation>6</tiff:Orientation>`).
pub(crate) fn xmp_digits(packet: &[u8]) -> Vec<usize> {
    let mut at = Vec::new();
    for (open, close) in [
        (&b"tiff:Orientation=\""[..], b'"'),
        (&b"<tiff:Orientation>"[..], b'<'),
    ] {
        let mut from = 0;
        while let Some(i) = find(&packet[from..], open) {
            let digit = from + i + open.len();
            if packet.get(digit).is_some_and(u8::is_ascii_digit)
                && packet.get(digit + 1) == Some(&close)
            {
                at.push(digit);
            }
            from = digit;
        }
    }
    at
}

pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TIFF block with a width and the orientation in its first IFD, in
    /// either byte order.
    fn tiff(big_endian: bool, orientation: u16) -> Vec<u8> {
        let u16b = |v: u16| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut t = Vec::new();
        t.extend(if big_endian { *b"MM" } else { *b"II" });
        t.extend(u16b(42));
        t.extend(u32b(8));
        t.extend(u16b(2));
        // ImageWidth, LONG, 1, 64
        t.extend(u16b(0x0100));
        t.extend(u16b(4));
        t.extend(u32b(1));
        t.extend(u32b(64));
        // Orientation, SHORT, 1, left-justified in the value field
        t.extend(u16b(0x0112));
        t.extend(u16b(3));
        t.extend(u32b(1));
        t.extend(u16b(orientation));
        t.extend([0, 0]);
        t.extend(u32b(0));
        t
    }

    fn segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend(u16::try_from(data.len() + 2).unwrap().to_be_bytes());
        s.extend(data);
        s
    }

    /// A JPEG's headers: JFIF, the EXIF and XMP segments given, then the
    /// start of the scan and a few bytes standing in for the picture.
    fn jpeg(exif: Option<Vec<u8>>, xmp: Option<&str>) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend(segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        if let Some(tiff) = exif {
            let mut data = b"Exif\0\0".to_vec();
            data.extend(tiff);
            j.extend(segment(0xE1, &data));
        }
        if let Some(packet) = xmp {
            let mut data = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
            data.extend(packet.as_bytes());
            j.extend(segment(0xE1, &data));
        }
        j.extend(segment(0xDA, &[1, 1, 0, 0, 63, 0]));
        // Scan data that looks like a tag, which must never be read as one.
        j.extend(b"tiff:Orientation=\"6\"\xFF\xD9");
        j
    }

    fn written(bytes: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        f
    }

    #[test]
    fn the_tag_is_read_in_either_byte_order() {
        for big_endian in [false, true] {
            let f = written(&jpeg(Some(tiff(big_endian, 6)), None));
            assert_eq!(read(f.path()), Orientation::from_exif(6), "{big_endian}");
        }
    }

    #[test]
    fn a_file_without_a_tag_or_not_a_jpeg_reads_as_none() {
        assert_eq!(read(written(&jpeg(None, None)).path()), None);
        assert_eq!(read(written(b"\x89PNG\r\n\x1a\n").path()), None);
        assert_eq!(read(written(&[]).path()), None);
        // A tag of a kind no writer uses is not trusted.
        let mut odd = tiff(false, 6);
        // The second entry's type, after the header, the count and the
        // first entry: SHORT (3) made LONG (4).
        assert_eq!(odd[24], 3);
        odd[24] = 4;
        assert_eq!(read(written(&jpeg(Some(odd), None)).path()), None);
    }

    #[test]
    fn resetting_writes_1_over_the_tag_and_nothing_else() {
        for big_endian in [false, true] {
            let before = jpeg(Some(tiff(big_endian, 8)), None);
            let f = written(&before);
            set_upright(f.path()).unwrap();
            let after = std::fs::read(f.path()).unwrap();
            assert_eq!(after.len(), before.len());
            let changed: Vec<usize> = (0..before.len())
                .filter(|&i| before[i] != after[i])
                .collect();
            assert_eq!(changed.len(), 1, "{changed:?}");
            assert_eq!(read(f.path()), Some(Orientation::UPRIGHT));
        }
    }

    #[test]
    fn resetting_also_resets_an_xmp_copy_of_the_tag() {
        let packet = r#"<x:xmpmeta><rdf:Description tiff:Orientation="6"/><tiff:Orientation>6</tiff:Orientation></x:xmpmeta>"#;
        let f = written(&jpeg(Some(tiff(false, 6)), Some(packet)));
        set_upright(f.path()).unwrap();
        let after = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).into_owned();
        assert!(after.contains(r#"tiff:Orientation="1"/>"#), "{after}");
        assert!(
            after.contains("<tiff:Orientation>1</tiff:Orientation>"),
            "{after}"
        );
        // The scan is left as it was.
        assert!(
            after.ends_with("tiff:Orientation=\"6\"\u{FFFD}\u{FFFD}"),
            "{after}"
        );
    }

    #[test]
    fn resetting_a_file_without_a_tag_is_an_error() {
        let f = written(&jpeg(None, None));
        assert!(set_upright(f.path()).is_err());
    }

    /// A file cut short or pointing past its end reads as having no tag,
    /// and is never read past its end.
    #[test]
    fn a_malformed_file_reads_as_none() {
        let whole = jpeg(Some(tiff(false, 6)), None);
        // Cut inside the EXIF segment, whose length then runs past the end.
        let cut = &whole[..30];
        assert_eq!(read(written(cut).path()), None);
        assert!(set_upright(written(cut).path()).is_err());
        // An IFD offset past the end of the TIFF block.
        let mut far = tiff(false, 6);
        far[4..8].copy_from_slice(&4000u32.to_le_bytes());
        assert_eq!(read(written(&jpeg(Some(far), None)).path()), None);
        // An entry count that runs past the block, searched to its end for
        // a tag that is not there.
        let mut many = tiff(true, 6);
        many[8..10].copy_from_slice(&500u16.to_be_bytes());
        many[22..24].copy_from_slice(&0x0113u16.to_be_bytes());
        assert_eq!(read(written(&jpeg(Some(many), None)).path()), None);
        // A segment length under its own two bytes.
        assert_eq!(read(written(b"\xFF\xD8\xFF\xE1\x00\x01").path()), None);
    }
}
