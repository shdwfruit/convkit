//! XMP copies of the orientation tag (`tiff:Orientation`), reset to 1 after
//! ImageMagick turns a picture. ImageMagick resets the EXIF tag when it
//! applies it (`-auto-orient`) but carries an XMP packet over as it was, so
//! a photo whose XMP said "turn a quarter right" would still say so on top
//! of pixels already turned. A plain file rewrite, no backend.
//!
//! jpg, webp, avif, heic, tiff and gif hold the packet as plain XML, with no
//! checksum over it, so the digit is overwritten where it is. png holds it
//! in a text chunk, compressed as ImageMagick writes it, which is decoded,
//! changed and written back with its checksum.

use std::io::{Read, Write};
use std::path::Path;

use crate::exif::{find, xmp_digits};
use crate::Format;

const OPEN: &[u8] = b"<x:xmpmeta";
const CLOSE: &[u8] = b"</x:xmpmeta>";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Resets every XMP copy of the orientation in a file of `format` to 1, in
/// place. A file with none, or one this cannot read, is left as it was.
pub(crate) fn set_upright(path: &Path, format: Format) -> std::io::Result<()> {
    let mut bytes = std::fs::read(path)?;
    let changed = match format {
        Format::Png => png(&bytes),
        _ => plain(&mut bytes).then_some(bytes),
    };
    match changed {
        Some(bytes) => std::fs::write(path, bytes),
        None => Ok(()),
    }
}

/// Resets the orientation in every XMP packet in `bytes`, from its
/// `<x:xmpmeta` to its `</x:xmpmeta>`. Whether any digit changed.
fn plain(bytes: &mut [u8]) -> bool {
    let mut changed = false;
    let mut from = 0;
    while let Some(i) = find(&bytes[from..], OPEN) {
        let start = from + i;
        let Some(len) = find(&bytes[start..], CLOSE) else {
            break;
        };
        let end = start + len + CLOSE.len();
        for d in xmp_digits(&bytes[start..end]) {
            if bytes[start + d] != b'1' {
                bytes[start + d] = b'1';
                changed = true;
            }
        }
        from = end;
    }
    changed
}

/// The PNG with the packet in each of its XMP text chunks reset, every other
/// chunk as it was. `None` when nothing changed, or the file does not read
/// as a PNG.
fn png(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let mut out = PNG_SIGNATURE.to_vec();
    let mut changed = false;
    let mut at = PNG_SIGNATURE.len();
    while at < bytes.len() {
        let len = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = bytes.get(at + 4..at + 8)?.try_into().ok()?;
        let end = at.checked_add(12)?.checked_add(len)?;
        let data = bytes.get(at + 8..end - 4)?;
        match text_chunk(&kind, data) {
            Some(new) => {
                out.extend(chunk(&kind, &new));
                changed = true;
            }
            None => out.extend(bytes.get(at..end)?),
        }
        at = end;
    }
    changed.then_some(out)
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = u32::try_from(data.len())
        .expect("a text chunk stays far below 4 GB")
        .to_be_bytes()
        .to_vec();
    c.extend(kind);
    c.extend(data);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    c.extend(crc.sum().to_be_bytes());
    c
}

/// A text chunk's data with its XMP packet reset: ImageMagick's raw profile
/// (`Raw profile type xmp`, in a zTXt or tEXt) or the standard iTXt
/// (`XML:com.adobe.xmp`). `None` for any other chunk, and one with nothing
/// to reset.
fn text_chunk(kind: &[u8; 4], data: &[u8]) -> Option<Vec<u8>> {
    let nul = data.iter().position(|&b| b == 0)?;
    let (keyword, rest) = (&data[..nul], &data[nul + 1..]);
    match (kind, keyword) {
        // The keyword, its NUL, the compression method, then the stream.
        (b"zTXt", b"Raw profile type xmp") => {
            let text = raw_profile(inflate(rest.get(1..)?)?)?;
            let mut out = data[..nul + 2].to_vec();
            out.extend(deflate(&text));
            Some(out)
        }
        (b"tEXt", b"Raw profile type xmp") => {
            let text = raw_profile(rest.to_vec())?;
            let mut out = data[..=nul].to_vec();
            out.extend(text);
            Some(out)
        }
        // The keyword and its NUL, a compression flag and method, then a
        // language tag and a translated keyword, each ending in a NUL.
        (b"iTXt", b"XML:com.adobe.xmp") => {
            let compressed = *rest.first()? == 1;
            let language = rest.get(2..)?.iter().position(|&b| b == 0)?;
            let translated = rest.get(3 + language..)?.iter().position(|&b| b == 0)?;
            let head = nul + 1 + 2 + language + 1 + translated + 1;
            let body = data.get(head..)?;
            let mut text = if compressed {
                inflate(body)?
            } else {
                body.to_vec()
            };
            if !plain(&mut text) {
                return None;
            }
            let mut out = data[..head].to_vec();
            out.extend(if compressed { deflate(&text) } else { text });
            Some(out)
        }
        _ => None,
    }
}

/// An ImageMagick raw profile with its packet reset: a blank line, the
/// profile's name and its length, each on a line, then its bytes as hex,
/// broken into lines. `None` when nothing changed.
fn raw_profile(mut text: Vec<u8>) -> Option<Vec<u8>> {
    let start = text
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == b'\n')
        .nth(2)?
        .0
        + 1;
    let hex: Vec<usize> = (start..text.len())
        .filter(|&i| text[i].is_ascii_hexdigit())
        .collect();
    let digit = |i: usize| char::from(text[i]).to_digit(16).map(|d| d as u8);
    let mut bytes = hex
        .chunks_exact(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect::<Option<Vec<u8>>>()?;
    if !plain(&mut bytes) {
        return None;
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (b, p) in bytes.iter().zip(hex.chunks_exact(2)) {
        text[p[0]] = HEX[usize::from(b >> 4)];
        text[p[1]] = HEX[usize::from(b & 15)];
    }
    Some(text)
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).expect("writing to memory");
    e.finish().expect("writing to memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKET: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description tiff:Orientation="6" xmp:Rating="4"/><tiff:Orientation>8</tiff:Orientation></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;

    fn written(bytes: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        f
    }

    #[test]
    fn a_plain_packet_is_reset_where_it_is() {
        let mut file = b"RIFF\0\0\0\0WEBPVP8 junk tiff:Orientation=\"6\" XMP ".to_vec();
        file.extend(PACKET.as_bytes());
        file.extend(b" trailer");
        let f = written(&file);
        set_upright(f.path(), Format::Webp).unwrap();
        let after = String::from_utf8(std::fs::read(f.path()).unwrap()).unwrap();
        assert_eq!(after.len(), file.len());
        assert!(
            after.contains(r#"tiff:Orientation="1" xmp:Rating="4""#),
            "{after}"
        );
        assert!(
            after.contains("<tiff:Orientation>1</tiff:Orientation>"),
            "{after}"
        );
        // Outside a packet, nothing is read as a tag.
        assert!(after.contains("junk tiff:Orientation=\"6\" XMP"), "{after}");
    }

    #[test]
    fn a_file_without_a_packet_is_left_as_it_was() {
        let f = written(b"\xFF\xD8\xFF\xD9");
        set_upright(f.path(), Format::Jpg).unwrap();
        assert_eq!(std::fs::read(f.path()).unwrap(), b"\xFF\xD8\xFF\xD9");
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        c.extend(kind);
        c.extend(data);
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        c.extend(crc.sum().to_be_bytes());
        c
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn unzlib(data: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    /// ImageMagick's raw profile: a name, the byte count, then the bytes as
    /// hex, 36 to a line, so a digit's two hex characters may straddle one.
    fn raw_profile(bytes: &[u8]) -> Vec<u8> {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let lines: Vec<&str> = hex
            .as_bytes()
            .chunks(72)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!("\nxmp\n{:8}\n{}\n", bytes.len(), lines.join("\n")).into_bytes()
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        p.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]));
        for c in chunks {
            p.extend(c);
        }
        p.extend(chunk(b"IEND", &[]));
        p
    }

    /// Each chunk of a PNG, with its checksum checked.
    fn chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 8;
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = png[at + 4..at + 8].try_into().unwrap();
            let data = png[at + 8..at + 8 + len].to_vec();
            let mut crc = flate2::Crc::new();
            crc.update(&kind);
            crc.update(&data);
            let stored = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            assert_eq!(stored, crc.sum(), "{}", String::from_utf8_lossy(&kind));
            out.push((kind, data));
            at += 12 + len;
        }
        out
    }

    #[test]
    fn a_png_raw_profile_is_decoded_reset_and_written_back() {
        let mut ztxt = b"Raw profile type xmp\0\0".to_vec();
        ztxt.extend(zlib(&raw_profile(PACKET.as_bytes())));
        let before = png(&[chunk(b"zTXt", &ztxt), chunk(b"tEXt", b"Author\0me")]);
        let f = written(&before);
        set_upright(f.path(), Format::Png).unwrap();
        let after = std::fs::read(f.path()).unwrap();
        let all = chunks(&after);
        let kinds: Vec<&[u8; 4]> = all.iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, [b"IHDR", b"zTXt", b"tEXt", b"IEND"]);
        let data = &all[1].1;
        let text = unzlib(&data[b"Raw profile type xmp\0\0".len()..]);
        let hex: String = String::from_utf8(text).unwrap().lines().skip(3).collect();
        let packet: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let packet = String::from_utf8(packet).unwrap();
        assert!(packet.contains(r#"tiff:Orientation="1""#), "{packet}");
        assert!(packet.contains("<tiff:Orientation>1</"), "{packet}");
        assert_eq!(all[2].1, b"Author\0me");
    }

    #[test]
    fn a_png_itxt_packet_is_reset_compressed_or_not() {
        for compressed in [false, true] {
            let mut itxt = b"XML:com.adobe.xmp\0".to_vec();
            itxt.extend([u8::from(compressed), 0, 0, 0]);
            itxt.extend(if compressed {
                zlib(PACKET.as_bytes())
            } else {
                PACKET.as_bytes().to_vec()
            });
            let f = written(&png(&[chunk(b"iTXt", &itxt)]));
            set_upright(f.path(), Format::Png).unwrap();
            let all = chunks(&std::fs::read(f.path()).unwrap());
            let data = &all[1].1;
            let body = &data[b"XML:com.adobe.xmp\0".len() + 4..];
            let text = if compressed {
                unzlib(body)
            } else {
                body.to_vec()
            };
            let text = String::from_utf8(text).unwrap();
            assert!(
                text.contains(r#"tiff:Orientation="1""#),
                "{compressed}: {text}"
            );
        }
    }

    #[test]
    fn a_malformed_png_is_left_as_it_was() {
        let mut broken = png(&[]);
        broken.truncate(20);
        let f = written(&broken);
        set_upright(f.path(), Format::Png).unwrap();
        assert_eq!(std::fs::read(f.path()).unwrap(), broken);
    }
}
