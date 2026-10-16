//! An mp4 or mov video track's display matrix, read and reset in place. A
//! plain file read, no backend.
//!
//! The matrix lives in each track's header (`moov/trak/tkhd`), next to a
//! movie-wide one (`moov/mvhd`) that players apply on top. Only the movie
//! box is read, however large the file: everything else is skipped by
//! seeking past it. Resetting overwrites 36 bytes per video track, so
//! nothing else in the file moves.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::rotate::Orientation;

/// The identity matrix: 16.16 fixed point, but 2.30 for the last column.
const IDENTITY: [i32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// A movie box larger than this is not read: a real one, even for hours of
/// video, is a few megabytes of sample tables.
const MAX_MOOV: u64 = 256 << 20;

/// What the first video track shows, when it is one of the eight plain
/// orientations and the movie-wide matrix adds nothing. `None` otherwise,
/// and for a file that is not an mp4 or mov.
pub(crate) fn read(path: &Path) -> Option<Orientation> {
    let movie = locate(&mut BufReader::new(File::open(path).ok()?)).ok()??;
    if movie.matrix != IDENTITY {
        return None;
    }
    let first = movie.video.first()?;
    let [a, b, _, c, d, ..] = first.matrix;
    Orientation::from_matrix([a, b, c, d])
}

/// Writes the identity over every video track's matrix: shown as stored.
pub(crate) fn set_upright(path: &Path) -> io::Result<()> {
    let mut f = File::options().read(true).write(true).open(path)?;
    let movie = locate(&mut BufReader::new(&f))?
        .filter(|m| !m.video.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "it has no video track"))?;
    let identity: Vec<u8> = IDENTITY.iter().flat_map(|v| v.to_be_bytes()).collect();
    for track in movie.video {
        f.seek(SeekFrom::Start(track.at))?;
        f.write_all(&identity)?;
    }
    f.flush()
}

/// The movie-wide matrix, and every video track's.
struct Movie {
    matrix: [i32; 9],
    video: Vec<Track>,
}

struct Track {
    /// The file offset of the track's matrix.
    at: u64,
    matrix: [i32; 9],
}

/// A box's type and where its body runs, as offsets into whatever was read.
struct Header {
    kind: [u8; 4],
    body: u64,
    end: u64,
}

/// Finds the top-level `moov`, reads it, and reads the matrices out of it.
fn locate<R: Read + Seek>(r: &mut R) -> io::Result<Option<Movie>> {
    let len = r.seek(SeekFrom::End(0))?;
    let mut at = 0;
    while at < len {
        r.seek(SeekFrom::Start(at))?;
        let mut head = [0; 16];
        let n = read_up_to(r, &mut head)?;
        let Some(h) = header(&head[..n], at, len) else {
            return Ok(None);
        };
        if &h.kind == b"moov" {
            if h.end - h.body > MAX_MOOV {
                return Ok(None);
            }
            let mut moov = vec![0; (h.end - h.body) as usize];
            r.seek(SeekFrom::Start(h.body))?;
            r.read_exact(&mut moov)?;
            return Ok(movie(&moov, h.body));
        }
        at = h.end;
    }
    Ok(None)
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The box header at `at`, given its first bytes, in a span ending at
/// `end`: a 32-bit size, 1 for a 64-bit size after the type, or 0 for "to
/// the end".
fn header(bytes: &[u8], at: u64, end: u64) -> Option<Header> {
    let size = u64::from(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?));
    let kind: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    let (size, head) = match size {
        0 => (end - at, 8),
        1 => (u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?), 16),
        s => (s, 8),
    };
    let box_end = at.checked_add(size)?;
    (size >= head && box_end <= end).then_some(Header {
        kind,
        body: at + head,
        end: box_end,
    })
}

/// The children of the box body `bytes[from..to]`.
fn children(bytes: &[u8], from: usize, to: usize) -> Vec<Header> {
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        let Some(h) = header(&bytes[at..to], at as u64, to as u64) else {
            break;
        };
        at = h.end as usize;
        out.push(h);
    }
    out
}

/// The matrices in a movie box's body, read from `moov`, which starts at
/// file offset `base`.
fn movie(moov: &[u8], base: u64) -> Option<Movie> {
    let mut matrix = None;
    let mut video = Vec::new();
    for b in children(moov, 0, moov.len()) {
        let (from, to) = (b.body as usize, b.end as usize);
        match &b.kind {
            // version, flags, then two times and a duration of 4 bytes each
            // (8 in version 1) around a 4-byte timescale, then the rate,
            // volume and reserved bytes.
            b"mvhd" => {
                let times = if moov.get(from)? == &1 { 28 } else { 16 };
                matrix = Some(matrix_at(moov, from + 4 + times + 16)?);
            }
            b"trak" => {
                let mut tkhd = None;
                let mut handler = None;
                for t in children(moov, from, to) {
                    match &t.kind {
                        b"tkhd" => tkhd = Some(t.body as usize),
                        b"mdia" => {
                            handler = children(moov, t.body as usize, t.end as usize)
                                .into_iter()
                                .find(|m| &m.kind == b"hdlr")
                                .and_then(|m| moov.get(m.body as usize + 8..m.body as usize + 12));
                        }
                        _ => {}
                    }
                }
                if let (Some(tkhd), Some(b"vide")) = (tkhd, handler) {
                    // version, flags, then the times, the track ID and the
                    // duration (32 bytes in version 1), then layer, volume
                    // and reserved bytes.
                    let times = if moov.get(tkhd)? == &1 { 32 } else { 20 };
                    let at = tkhd + 4 + times + 16;
                    video.push(Track {
                        at: base + at as u64,
                        matrix: matrix_at(moov, at)?,
                    });
                }
            }
            _ => {}
        }
    }
    Some(Movie {
        matrix: matrix?,
        video,
    })
}

fn matrix_at(bytes: &[u8], at: usize) -> Option<[i32; 9]> {
    let raw = bytes.get(at..at + 36)?;
    let mut m = [0; 9];
    for (i, v) in m.iter_mut().enumerate() {
        *v = i32::from_be_bytes(raw[i * 4..i * 4 + 4].try_into().ok()?);
    }
    Some(m)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const ONE: i32 = 0x10000;
    pub(crate) const RIGHT: [i32; 9] = [0, ONE, 0, -ONE, 0, 0, 0, 0, 1 << 30];
    pub(crate) const IDENTITY: [i32; 9] = [ONE, 0, 0, 0, ONE, 0, 0, 0, 1 << 30];

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        b.extend(kind);
        b.extend(body);
        b
    }

    fn matrix(m: [i32; 9]) -> Vec<u8> {
        m.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    fn mvhd(m: [i32; 9]) -> Vec<u8> {
        let mut body = vec![0; 4 + 16 + 16];
        body.extend(matrix(m));
        body.extend([0; 28]);
        boxed(b"mvhd", &body)
    }

    /// A track header, version 0 or 1 (64-bit times), then its media's
    /// handler.
    pub(crate) fn trak(version: u8, m: [i32; 9], handler: &[u8; 4]) -> Vec<u8> {
        let mut tkhd = vec![version, 0, 0, 0];
        tkhd.extend(vec![0; if version == 1 { 32 } else { 20 }]);
        tkhd.extend([0; 16]);
        tkhd.extend(matrix(m));
        tkhd.extend((320u32 << 16).to_be_bytes());
        tkhd.extend((180u32 << 16).to_be_bytes());
        let mut hdlr = vec![0; 8];
        hdlr.extend(handler);
        hdlr.extend([0; 13]);
        let mdia = boxed(b"mdia", &boxed(b"hdlr", &hdlr));
        let mut body = boxed(b"tkhd", &tkhd);
        body.extend(mdia);
        boxed(b"trak", &body)
    }

    /// `ftyp`, a large-size `mdat` ahead of the movie box, as an unoptimised
    /// mp4 has it, then `moov` with the tracks given.
    pub(crate) fn movie(movie_matrix: [i32; 9], tracks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = boxed(b"ftyp", b"isom\0\0\x02\0isommp41");
        let payload = [0xAB; 100];
        f.extend(1u32.to_be_bytes());
        f.extend(b"mdat");
        f.extend((payload.len() as u64 + 16).to_be_bytes());
        f.extend(payload);
        let mut moov = mvhd(movie_matrix);
        for t in tracks {
            moov.extend(t);
        }
        f.extend(boxed(b"moov", &moov));
        f
    }

    fn written(bytes: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        f
    }

    #[test]
    fn the_first_video_tracks_matrix_is_read() {
        let file = movie(
            IDENTITY,
            &[trak(0, IDENTITY, b"soun"), trak(1, RIGHT, b"vide")],
        );
        assert_eq!(read(written(&file).path()), Orientation::from_exif(6));
    }

    #[test]
    fn a_file_that_is_not_a_movie_or_shows_no_plain_turn_reads_as_none() {
        assert_eq!(read(written(b"\xFF\xD8\xFF\xE0").path()), None);
        assert_eq!(read(written(&movie(IDENTITY, &[])).path()), None);
        // A movie-wide matrix is applied on top of the track's; conv does
        // not try to work out what the two show together.
        let turned = movie(RIGHT, &[trak(0, IDENTITY, b"vide")]);
        assert_eq!(read(written(&turned).path()), None);
        let scaled = [2 * ONE, 0, 0, 0, ONE, 0, 0, 0, 1 << 30];
        let file = movie(IDENTITY, &[trak(0, scaled, b"vide")]);
        assert_eq!(read(written(&file).path()), None);
    }

    #[test]
    fn resetting_writes_the_identity_into_every_video_track_and_nothing_else() {
        let before = movie(
            IDENTITY,
            &[
                trak(0, RIGHT, b"vide"),
                trak(0, RIGHT, b"soun"),
                trak(1, RIGHT, b"vide"),
            ],
        );
        let f = written(&before);
        set_upright(f.path()).unwrap();
        let after = std::fs::read(f.path()).unwrap();
        assert_eq!(after.len(), before.len());
        let identity = matrix(IDENTITY);
        let right = matrix(RIGHT);
        let count = |bytes: &[u8], m: &[u8]| bytes.windows(36).filter(|w| *w == m).count();
        // The movie header's and both video tracks' are the identity; the
        // sound track's is left as it was.
        assert_eq!(count(&after, &identity), 3);
        assert_eq!(count(&after, &right), 1);
        let differs = (0..before.len()).filter(|&i| before[i] != after[i]).count();
        assert!(differs <= 2 * 36, "{differs}");
        assert_eq!(read(f.path()), Some(Orientation::UPRIGHT));
    }

    #[test]
    fn resetting_a_file_without_a_video_track_is_an_error() {
        let f = written(&movie(IDENTITY, &[trak(0, RIGHT, b"soun")]));
        assert!(set_upright(f.path()).is_err());
    }

    /// Box sizes that cannot be true read as no movie, and are never read
    /// past the end.
    #[test]
    fn a_malformed_file_reads_as_none() {
        // A box claiming to be smaller than its own header.
        let mut tiny = 4u32.to_be_bytes().to_vec();
        tiny.extend(b"moov");
        tiny.extend([0; 16]);
        assert_eq!(read(written(&tiny).path()), None);
        // A movie box larger than the file.
        let mut big = movie(IDENTITY, &[trak(0, RIGHT, b"vide")]);
        let at = big.windows(4).position(|w| w == b"moov").unwrap() - 4;
        big[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read(written(&big).path()), None);
        assert!(set_upright(written(&big).path()).is_err());
        // A file cut inside a track header.
        let whole = movie(IDENTITY, &[trak(0, RIGHT, b"vide")]);
        let at = whole.windows(4).position(|w| w == b"tkhd").unwrap() + 20;
        assert_eq!(read(written(&whole[..at]).path()), None);
    }
}
