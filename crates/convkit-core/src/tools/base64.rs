//! `conv base64`: base64 in both directions, and `data:` URIs.
//!
//! Decoding takes either alphabet, with or without padding, and ignores
//! line breaks, so wrapped base64 from a mail or a PEM file decodes as it
//! is. The alphabets are told apart by their own characters (`+ /` against
//! `- _`), which never overlap, so nothing is guessed; a text with both is
//! refused.

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;

/// Which of the two base64 alphabets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alphabet {
    /// `A-Z a-z 0-9 + /`, padded with `=`.
    Standard,
    /// `A-Z a-z 0-9 - _` (RFC 4648 section 5), as in URLs and JWTs.
    UrlSafe,
}

impl Alphabet {
    pub fn name(self) -> &'static str {
        match self {
            Alphabet::Standard => "standard",
            Alphabet::UrlSafe => "url-safe",
        }
    }
}

/// `bytes` as base64: padded in the standard alphabet, unpadded in the
/// URL-safe one, as URLs and JWTs carry it.
pub fn encode(bytes: &[u8], alphabet: Alphabet) -> String {
    match alphabet {
        Alphabet::Standard => STANDARD.encode(bytes),
        Alphabet::UrlSafe => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
    }
}

/// `bytes` as a `data:` URI of type `mime`, for pasting into HTML or CSS.
pub fn data_uri(bytes: &[u8], mime: &str) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// What decoding gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub bytes: Vec<u8>,
    /// The type a `data:` URI named, or `None` for plain base64.
    pub mime: Option<String>,
    /// The alphabet the text used, or `None` for a `data:` URI that was
    /// percent-encoded rather than base64.
    pub alphabet: Option<Alphabet>,
}

/// Decodes base64 in either alphabet, or a `data:` URI.
pub fn decode(input: &str) -> Result<Decoded, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("there is nothing to decode".to_string());
    }
    if text
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("data:"))
    {
        return decode_data_uri(text);
    }
    let (bytes, alphabet) = decode_base64(text)?;
    Ok(Decoded {
        bytes,
        mime: None,
        alphabet: Some(alphabet),
    })
}

/// `data:[TYPE][;base64],DATA`.
fn decode_data_uri(uri: &str) -> Result<Decoded, String> {
    let rest = &uri[5..];
    let Some((meta, data)) = rest.split_once(',') else {
        return Err("this data: URI has no comma, so it holds no data".to_string());
    };
    let (mime, is_base64) = match meta.len().checked_sub(7) {
        Some(at) if meta.is_char_boundary(at) && meta[at..].eq_ignore_ascii_case(";base64") => {
            (&meta[..at], true)
        }
        _ => (meta, false),
    };
    // A data: URI without a type is US-ASCII text (RFC 2397).
    let mime = if mime.is_empty() {
        "text/plain;charset=US-ASCII".to_string()
    } else {
        mime.to_string()
    };
    // Percent-escapes are allowed in either form; base64 never uses `%`.
    let unescaped: Vec<u8> = percent_encoding::percent_decode_str(data).collect();
    if !is_base64 {
        return Ok(Decoded {
            bytes: unescaped,
            mime: Some(mime),
            alphabet: None,
        });
    }
    let text = String::from_utf8(unescaped)
        .map_err(|_| "the data in this data: URI isn't base64".to_string())?;
    let (bytes, alphabet) = decode_base64(&text)?;
    Ok(Decoded {
        bytes,
        mime: Some(mime),
        alphabet: Some(alphabet),
    })
}

fn decode_base64(text: &str) -> Result<(Vec<u8>, Alphabet), String> {
    // Where each kept character was in what was given, for messages.
    let mut kept = String::with_capacity(text.len());
    let mut at = Vec::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let ok = c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_' | '=');
        if !ok {
            return Err(format!(
                "this isn't base64: {} at character {} isn't in either base64 alphabet",
                shown(c),
                i + 1
            ));
        }
        kept.push(c);
        at.push(i + 1);
    }
    let standard = kept.contains(['+', '/']);
    let url = kept.contains(['-', '_']);
    if standard && url {
        return Err(
            "this mixes the standard (+ /) and URL-safe (- _) base64 alphabets, so it is neither"
                .to_string(),
        );
    }
    let alphabet = if url {
        Alphabet::UrlSafe
    } else {
        Alphabet::Standard
    };
    let body = kept.trim_end_matches('=');
    let padding = kept.len() - body.len();
    if let Some(i) = body.find('=') {
        return Err(format!(
            "this isn't base64: = at character {} is padding, which only comes at the end",
            at[i]
        ));
    }
    if body.len() % 4 == 1 {
        return Err(format!(
            "this isn't whole base64: {} characters can't be, as every 4 make 3 bytes; a character is missing or one too many",
            body.len()
        ));
    }
    if padding > 0 && (kept.len() % 4 != 0 || padding > 2) {
        return Err("the = padding at the end is wrong for this length".to_string());
    }
    let engine = GeneralPurpose::new(
        match alphabet {
            Alphabet::Standard => &alphabet::STANDARD,
            Alphabet::UrlSafe => &alphabet::URL_SAFE,
        },
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let bytes = engine.decode(body).map_err(|e| match e {
        base64::DecodeError::InvalidLastSymbol(i, _) => format!(
            "this isn't whole base64: the last character, at {}, leaves bits over, so the text was probably cut short",
            at.get(i).copied().unwrap_or(i + 1)
        ),
        other => format!("this isn't base64: {other}"),
    })?;
    Ok((bytes, alphabet))
}

/// A character as it can be shown in a message: quoted when it is plain
/// ASCII, and otherwise by code point and name, since it may not show at
/// all (a zero-width space pasted in with the text).
fn shown(c: char) -> String {
    if c.is_ascii_graphic() {
        format!("{c:?}")
    } else {
        format!("U+{:04X} {}", c as u32, super::chars::name(c))
    }
}

/// The MIME type for an extension, for the types a data URI or a decoded
/// file is likely to be.
const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("ico", "image/x-icon"),
    ("bmp", "image/bmp"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("tiff", "image/tiff"),
    ("pdf", "application/pdf"),
    ("json", "application/json"),
    ("txt", "text/plain"),
    ("html", "text/html"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("xml", "application/xml"),
    ("csv", "text/csv"),
    ("md", "text/markdown"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("wasm", "application/wasm"),
];

/// Other spellings of the extensions in `TYPES`.
const ALIASES: &[(&str, &str)] = &[
    ("jpeg", "jpg"),
    ("jpe", "jpg"),
    ("tif", "tiff"),
    ("htm", "html"),
    ("mjs", "js"),
    ("text", "txt"),
    ("markdown", "md"),
    ("heif", "heic"),
];

/// The MIME type for a file extension (`png`, `JPEG`), if it is a common
/// one.
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    let ext = ext.to_ascii_lowercase();
    let ext = ALIASES
        .iter()
        .find(|(a, _)| *a == ext)
        .map_or(ext.as_str(), |(_, e)| e);
    TYPES.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// The extension for a MIME type, ignoring its parameters
/// (`text/plain;charset=utf-8` is `txt`).
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let base = match base.as_str() {
        "image/jpg" | "image/pjpeg" => "image/jpeg",
        "image/vnd.microsoft.icon" => "image/x-icon",
        "application/javascript" | "application/x-javascript" => "text/javascript",
        "text/xml" => "application/xml",
        "audio/mp3" => "audio/mpeg",
        "audio/x-wav" | "audio/wave" => "audio/wav",
        "image/heif" => "image/heic",
        other => other,
    };
    TYPES.iter().find(|(_, m)| *m == base).map(|(e, _)| *e)
}

/// The type of `bytes` from their first bytes, for the formats with a
/// signature. Text is `None`: a signature can't tell CSS from plain text.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.starts_with(sig);
    let at = |offset: usize, sig: &[u8]| bytes.get(offset..offset + sig.len()) == Some(sig);
    let mime = if starts(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if starts(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        "image/gif"
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if starts(b"RIFF") && at(8, b"WAVE") {
        "audio/wav"
    } else if starts(b"%PDF-") {
        "application/pdf"
    } else if starts(b"\0\0\x01\0") && bytes.len() > 6 {
        "image/x-icon"
    } else if starts(b"BM") && bytes.len() > 26 && at(6, b"\0\0\0\0") {
        "image/bmp"
    } else if starts(b"II*\0") || starts(b"MM\0*") {
        "image/tiff"
    } else if at(4, b"ftyp") {
        match bytes.get(8..12) {
            Some(b"avif") | Some(b"avis") => "image/avif",
            Some(b"heic") | Some(b"heix") | Some(b"mif1") | Some(b"msf1") => "image/heic",
            _ => "video/mp4",
        }
    } else if starts(b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else if starts(b"OggS") {
        "audio/ogg"
    } else if starts(b"ID3") || starts(b"\xff\xfb") || starts(b"\xff\xf3") {
        "audio/mpeg"
    } else if starts(b"PK\x03\x04") {
        "application/zip"
    } else if starts(b"\x1f\x8b") {
        "application/gzip"
    } else if starts(b"wOFF") {
        "font/woff"
    } else if starts(b"wOF2") {
        "font/woff2"
    } else if starts(b"\0asm") {
        "application/wasm"
    } else if looks_like_svg(bytes) {
        "image/svg+xml"
    } else {
        return None;
    };
    Some(mime)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let Ok(head) =
        std::str::from_utf8(head).or_else(|e| std::str::from_utf8(&head[..e.valid_up_to()]))
    else {
        return false;
    };
    let head = head.trim_start_matches('\u{feff}').trim_start();
    (head.starts_with("<svg") || head.starts_with("<?xml") || head.starts_with("<!--"))
        && head.contains("<svg")
}

/// The type to give `bytes` in a data URI: from its signature, then the
/// extension of the file it came from, then text or binary.
pub fn mime_for(bytes: &[u8], ext: Option<&str>) -> String {
    if let Some(m) = sniff(bytes) {
        return m.to_string();
    }
    if let Some(m) = ext.and_then(mime_for_ext) {
        return if m.starts_with("text/") && !bytes.is_ascii() {
            format!("{m};charset=utf-8")
        } else {
            m.to_string()
        };
    }
    match std::str::from_utf8(bytes) {
        Ok(s) if !is_text(s) => "application/octet-stream".to_string(),
        Ok(s) if s.is_ascii() => "text/plain".to_string(),
        Ok(_) => "text/plain;charset=utf-8".to_string(),
        Err(_) => "application/octet-stream".to_string(),
    }
}

/// UTF-8 with no control characters but line breaks, tabs and form
/// feeds: something a terminal can show.
pub fn is_text(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_in_both_alphabets() {
        assert_eq!(encode(b"hello", Alphabet::Standard), "aGVsbG8=");
        assert_eq!(encode(b"\xfb\xff", Alphabet::Standard), "+/8=");
        assert_eq!(encode(b"\xfb\xff", Alphabet::UrlSafe), "-_8");
    }

    #[test]
    fn decodes_either_alphabet_with_or_without_padding() {
        let d = decode("aGVsbG8=").unwrap();
        assert_eq!(d.bytes, b"hello");
        assert_eq!(d.alphabet, Some(Alphabet::Standard));
        assert_eq!(decode("aGVsbG8").unwrap().bytes, b"hello");
        let d = decode("-_8").unwrap();
        assert_eq!(d.bytes, b"\xfb\xff");
        assert_eq!(d.alphabet, Some(Alphabet::UrlSafe));
        assert_eq!(decode("+/8=").unwrap().bytes, b"\xfb\xff");
    }

    #[test]
    fn wrapped_lines_decode() {
        let wrapped = "aGVs\r\nbG8g\n d29y\tbGQ=\n";
        assert_eq!(decode(wrapped).unwrap().bytes, b"hello world");
    }

    #[test]
    fn a_character_outside_both_alphabets_is_named_with_its_place() {
        let e = decode("aGV!sbG8=").unwrap_err();
        assert!(e.contains("'!' at character 4"), "{e}");
        let e = decode("aGV\u{200b}sbG8=").unwrap_err();
        assert!(e.contains("U+200B ZERO WIDTH SPACE at character 4"), "{e}");
    }

    #[test]
    fn mixed_alphabets_are_refused() {
        let e = decode("ab+-").unwrap_err();
        assert!(e.contains("mixes"), "{e}");
    }

    #[test]
    fn padding_in_the_middle_or_a_wrong_length_is_refused() {
        assert!(decode("aG=VsbG8")
            .unwrap_err()
            .contains("only comes at the end"));
        assert!(decode("aGVsb").unwrap_err().contains("5 characters"));
        assert!(decode("aGVsbG8==").unwrap_err().contains("padding"));
        // Bits left over in the last character: cut short.
        assert!(decode("aGVsbG9").unwrap_err().contains("cut short"));
        assert!(decode("   ").unwrap_err().contains("nothing to decode"));
    }

    #[test]
    fn data_uris_decode_with_their_type() {
        let d = decode("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(d.mime.as_deref(), Some("image/png"));
        assert_eq!(d.bytes, b"\x89PNG\r\n\x1a\n");
        let d = decode("DATA:text/plain;BASE64,aGk=").unwrap();
        assert_eq!(d.bytes, b"hi");
        // Not base64: percent-encoded.
        let d = decode("data:,Hello%2C%20World").unwrap();
        assert_eq!(d.bytes, b"Hello, World");
        assert_eq!(d.mime.as_deref(), Some("text/plain;charset=US-ASCII"));
        assert_eq!(d.alphabet, None);
        assert!(decode("data:image/png;base64")
            .unwrap_err()
            .contains("no comma"));
    }

    #[test]
    fn a_data_uri_round_trips() {
        let uri = data_uri(b"GIF89a....", "image/gif");
        assert!(uri.starts_with("data:image/gif;base64,R0lGODlh"));
        assert_eq!(decode(&uri).unwrap().bytes, b"GIF89a....");
    }

    #[test]
    fn types_come_from_signatures_then_extensions() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"\0\0\0\x1cftypavif"), Some("image/avif"));
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?>\n<svg xmlns="),
            Some("image/svg+xml")
        );
        assert_eq!(sniff(b"body { color: red }"), None);
        assert_eq!(mime_for(b"body { color: red }", Some("css")), "text/css");
        assert_eq!(
            mime_for("héllo".as_bytes(), None),
            "text/plain;charset=utf-8"
        );
        assert_eq!(mime_for(b"\x00\x01\x02", None), "application/octet-stream");
        assert_eq!(mime_for(b"\x89PNG\r\n\x1a\n", Some("txt")), "image/png");
    }

    #[test]
    fn extensions_and_types_map_both_ways() {
        assert_eq!(mime_for_ext("JPEG"), Some("image/jpeg"));
        assert_eq!(ext_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(ext_for_mime("text/plain;charset=utf-8"), Some("txt"));
        assert_eq!(ext_for_mime("image/vnd.microsoft.icon"), Some("ico"));
        assert_eq!(ext_for_mime("application/x-unknown"), None);
    }
}
