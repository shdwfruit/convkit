//! Byte-level synthesis of two pieces of metadata ImageMagick has no
//! command-line way to fabricate from scratch: a minimal EXIF `Orientation`
//! tag (for a JPEG that otherwise carries no EXIF at all) and a minimal,
//! structurally valid, deliberately-not-sRGB ICC profile.
//!
//! Both were hand-verified against this project's actual ImageMagick 7.1.2
//! build before being written here: the EXIF segment is read back correctly
//! by `identify -format "%[EXIF:Orientation]"`/`%[orientation]`, and the ICC
//! profile round-trips byte-for-byte through `-profile <file>` and
//! `icc:-` extraction (only cosmetic libpng warnings, not a rejection),
//! with `%[icc:description]` reading back the embedded description text.

/// A minimal EXIF APP1 segment carrying exactly one IFD0 tag --
/// `Orientation` (0x0112, SHORT, count 1) -- as a raw byte sequence ready to
/// be spliced into a JPEG immediately after its SOI marker (`FF D8`).
///
/// Structure: `FF E1 <len> "Exif\0\0" <TIFF header> <IFD0: 1 entry> <next
/// IFD offset = 0>`. Little-endian ("II") byte order throughout. This is
/// deliberately the smallest EXIF blob that says anything at all -- no
/// thumbnail, no other tags -- which is exactly what's needed to prove a
/// converter reads (or fails to read) this one tag.
pub fn exif_orientation_app1(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag: Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // type: SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count: 1
                                                 // Value field is 4 bytes; a SHORT value is left-justified (low bytes)
                                                 // for "II" byte order, remaining 2 bytes unused.
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset: none

    let mut body = Vec::new();
    body.extend_from_slice(b"Exif\0\0");
    body.extend_from_slice(&tiff);

    let mut seg = Vec::new();
    seg.extend_from_slice(&[0xFF, 0xE1]);
    seg.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    seg.extend_from_slice(&body);
    seg
}

/// Splices `exif_orientation_app1(orientation)` into `jpeg` right after its
/// `FF D8` SOI marker. `jpeg` must start with a valid SOI (true of every
/// JPEG ImageMagick writes); returns the bytes unchanged with a note in the
/// `Err` case rather than panicking, so a caller can surface a clear error
/// instead of corrupting a file silently.
pub fn inject_exif_orientation(jpeg: &[u8], orientation: u16) -> Result<Vec<u8>, String> {
    if jpeg.len() < 2 || jpeg[0..2] != [0xFF, 0xD8] {
        return Err("input does not start with a JPEG SOI marker (FF D8)".to_string());
    }
    let mut out = Vec::with_capacity(jpeg.len() + 40);
    out.extend_from_slice(&jpeg[0..2]);
    out.extend_from_slice(&exif_orientation_app1(orientation));
    out.extend_from_slice(&jpeg[2..]);
    Ok(out)
}

/// Exact ICC PCS illuminant (D50) as `s15Fixed16Number`, per the ICC spec's
/// Annex D reference values -- required exactly, not merely "close", or
/// libpng's iCCP chunk validator warns (`PCS illuminant is not D50`).
const D50_X: i32 = 0x0000_F6D6;
const D50_Y: i32 = 0x0001_0000;
const D50_Z: i32 = 0x0000_D32D;

fn pad4(mut b: Vec<u8>) -> Vec<u8> {
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b
}

fn s15fixed16(v: f64) -> i32 {
    (v * 65536.0).round() as i32
}

fn xyz_type(x: i32, y: i32, z: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"XYZ ");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&x.to_be_bytes());
    body.extend_from_slice(&y.to_be_bytes());
    body.extend_from_slice(&z.to_be_bytes());
    pad4(body)
}

fn text_desc(text: &str) -> Vec<u8> {
    let mut ascii = text.as_bytes().to_vec();
    ascii.push(0);
    let mut body = Vec::new();
    body.extend_from_slice(b"desc");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&(ascii.len() as u32).to_be_bytes());
    body.extend_from_slice(&ascii);
    body.extend_from_slice(&0u32.to_be_bytes()); // unicode language code
    body.extend_from_slice(&0u32.to_be_bytes()); // unicode count
    body.extend_from_slice(&0u16.to_be_bytes()); // scriptcode code
    body.extend(std::iter::repeat_n(0u8, 67)); // scriptcode 67-byte macstring
    pad4(body)
}

fn text_type(text: &str) -> Vec<u8> {
    let mut ascii = text.as_bytes().to_vec();
    ascii.push(0);
    let mut body = Vec::new();
    body.extend_from_slice(b"text");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&ascii);
    pad4(body)
}

fn curv_gamma(gamma: f64) -> Vec<u8> {
    let val = (gamma * 256.0).round() as u16;
    let mut body = Vec::new();
    body.extend_from_slice(b"curv");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&val.to_be_bytes());
    pad4(body)
}

/// A minimal, structurally valid ICC v2 RGB display profile, carrying just
/// enough tags (`desc`, `cprt`, `wtpt`, `rXYZ`/`gXYZ`/`bXYZ`,
/// `rTRC`/`gTRC`/`bTRC`) to be a legitimate embeddable profile -- and
/// deliberately not sRGB: gamma 1.8 (not sRGB's ~2.2 piecewise curve) and
/// primaries that don't match sRGB's, so a converter that silently drops
/// this profile and one that silently substitutes sRGB are both
/// detectable, not just "some ICC profile is present".
pub fn synthetic_non_srgb_icc(description: &str) -> Vec<u8> {
    let desc = text_desc(description);
    let cprt = text_type("Public domain, synthesised for convkit-diff testing");
    let wtpt = xyz_type(D50_X, D50_Y, D50_Z);
    // Deliberately not sRGB's primaries (0.64/0.33 red, 0.30/0.60 green,
    // 0.15/0.06 blue in xy chromaticity) -- these are wide-gamut-ish
    // (ROMM/ProPhoto-like) values instead.
    let rxyz = xyz_type(s15fixed16(0.7347), s15fixed16(0.2653), 0);
    let gxyz = xyz_type(s15fixed16(0.1596), s15fixed16(0.8404), 0);
    let bxyz = xyz_type(s15fixed16(0.0366), s15fixed16(0.0001), 0);
    let trc = curv_gamma(1.8);

    let tags: Vec<(&[u8; 4], &[u8])> = vec![
        (b"desc", &desc),
        (b"cprt", &cprt),
        (b"wtpt", &wtpt),
        (b"rXYZ", &rxyz),
        (b"gXYZ", &gxyz),
        (b"bXYZ", &bxyz),
        (b"rTRC", &trc),
        (b"gTRC", &trc),
        (b"bTRC", &trc),
    ];

    let header_size = 128usize;
    let tag_table_size = 4 + tags.len() * 12;
    let mut offset = header_size + tag_table_size;
    let mut tag_table = Vec::new();
    let mut tag_data = Vec::new();
    for (sig, data) in &tags {
        tag_table.extend_from_slice(*sig);
        tag_table.extend_from_slice(&(offset as u32).to_be_bytes());
        tag_table.extend_from_slice(&(data.len() as u32).to_be_bytes());
        tag_data.extend_from_slice(data);
        offset += data.len();
    }
    let total_size = header_size + tag_table_size + tag_data.len();

    let mut header = vec![0u8; header_size];
    header[0..4].copy_from_slice(&(total_size as u32).to_be_bytes());
    header[4..8].copy_from_slice(b"none"); // CMM type
    header[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes()); // version 2.1.0
    header[12..16].copy_from_slice(b"mntr"); // device class: display
    header[16..20].copy_from_slice(b"RGB "); // colour space
    header[20..24].copy_from_slice(b"XYZ "); // PCS
                                             // Date/time (24..36) left zero -- not load-bearing for any reader here.
    header[36..40].copy_from_slice(b"acsp"); // profile file signature
    header[48..52].copy_from_slice(b"none"); // device manufacturer
    header[52..56].copy_from_slice(b"none"); // device model
    header[64..68].copy_from_slice(&1u32.to_be_bytes()); // rendering intent
    header[68..72].copy_from_slice(&D50_X.to_be_bytes());
    header[72..76].copy_from_slice(&D50_Y.to_be_bytes());
    header[76..80].copy_from_slice(&D50_Z.to_be_bytes());
    header[80..84].copy_from_slice(b"none"); // profile creator

    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&header);
    out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    out.extend_from_slice(&tag_table);
    out.extend_from_slice(&tag_data);
    debug_assert_eq!(out.len(), total_size);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_segment_carries_the_requested_orientation_value() {
        let seg = exif_orientation_app1(6);
        assert_eq!(&seg[0..2], &[0xFF, 0xE1]);
        assert_eq!(&seg[4..10], b"Exif\0\0");
        // The value field of the one IFD0 entry, at a fixed offset in this
        // minimal, single-entry layout.
        let value_offset = 4 + 6 + 8 + 2 + 8; // marker+len, Exif\0\0, TIFF header, count, tag/type/count
        assert_eq!(seg[value_offset], 6);
    }

    #[test]
    fn injection_rejects_a_non_jpeg_input() {
        let e = inject_exif_orientation(b"not a jpeg", 1).unwrap_err();
        assert!(e.contains("SOI"));
    }

    #[test]
    fn injection_places_the_segment_immediately_after_soi() {
        let fake_jpeg = [0xFFu8, 0xD8, 0xFF, 0xD9]; // SOI, EOI
        let out = inject_exif_orientation(&fake_jpeg, 3).unwrap();
        assert_eq!(&out[0..2], &[0xFF, 0xD8]);
        assert_eq!(&out[2..4], &[0xFF, 0xE1]);
        assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9]);
    }

    #[test]
    fn icc_profile_has_a_valid_acsp_signature_and_declared_size() {
        let icc = synthetic_non_srgb_icc("test profile");
        assert_eq!(&icc[36..40], b"acsp");
        let declared_size = u32::from_be_bytes(icc[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared_size, icc.len());
        assert_eq!(&icc[16..20], b"RGB ");
    }

    #[test]
    fn icc_profile_embeds_the_description_text() {
        let icc = synthetic_non_srgb_icc("hello-description");
        let haystack = String::from_utf8_lossy(&icc);
        assert!(haystack.contains("hello-description"));
    }
}
