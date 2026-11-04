//! `conv hash`: checksums, and checking a file against one.
//!
//! A hash given to check names its own algorithm by its length (64 hex
//! digits can only be SHA-256), so `--check` never needs to be told which.

use std::io::Read;

use md5::Md5;
use sha1::Sha1;
use sha2::digest::DynDigest;
use sha2::{Sha256, Sha384, Sha512};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    pub const ALL: [Algorithm; 5] = [
        Algorithm::Md5,
        Algorithm::Sha1,
        Algorithm::Sha256,
        Algorithm::Sha384,
        Algorithm::Sha512,
    ];

    /// As people write it: SHA-256.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Md5 => "MD5",
            Algorithm::Sha1 => "SHA-1",
            Algorithm::Sha256 => "SHA-256",
            Algorithm::Sha384 => "SHA-384",
            Algorithm::Sha512 => "SHA-512",
        }
    }

    /// As a key or a flag: sha256.
    pub fn key(self) -> &'static str {
        match self {
            Algorithm::Md5 => "md5",
            Algorithm::Sha1 => "sha1",
            Algorithm::Sha256 => "sha256",
            Algorithm::Sha384 => "sha384",
            Algorithm::Sha512 => "sha512",
        }
    }

    /// How many hex digits its hash has.
    pub fn hex_len(self) -> usize {
        match self {
            Algorithm::Md5 => 32,
            Algorithm::Sha1 => 40,
            Algorithm::Sha256 => 64,
            Algorithm::Sha384 => 96,
            Algorithm::Sha512 => 128,
        }
    }

    pub fn from_hex_len(n: usize) -> Option<Algorithm> {
        Algorithm::ALL.into_iter().find(|a| a.hex_len() == n)
    }

    /// `sha256`, `SHA-256`, `sha_256` and the like.
    pub fn from_label(s: &str) -> Option<Algorithm> {
        let s: String = s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        Algorithm::ALL.into_iter().find(|a| a.key() == s)
    }

    /// Whether a match on this algorithm still says something against
    /// someone who chose the file to fool it.
    pub fn broken(self) -> bool {
        matches!(self, Algorithm::Md5 | Algorithm::Sha1)
    }

    fn hasher(self) -> Box<dyn DynDigest> {
        match self {
            Algorithm::Md5 => Box::new(Md5::default()),
            Algorithm::Sha1 => Box::new(Sha1::default()),
            Algorithm::Sha256 => Box::new(Sha256::default()),
            Algorithm::Sha384 => Box::new(Sha384::default()),
            Algorithm::Sha512 => Box::new(Sha512::default()),
        }
    }
}

/// Every hash of one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub bytes: u64,
    /// In the order asked for, as lowercase hex.
    pub hashes: Vec<(Algorithm, String)>,
}

impl Hashed {
    pub fn get(&self, algorithm: Algorithm) -> Option<&str> {
        self.hashes
            .iter()
            .find(|(a, _)| *a == algorithm)
            .map(|(_, h)| h.as_str())
    }
}

/// Reads `r` to its end once, feeding every algorithm, so a large file is
/// read once and never held in memory.
pub fn hash_reader(mut r: impl Read, algorithms: &[Algorithm]) -> std::io::Result<Hashed> {
    let mut hashers: Vec<(Algorithm, Box<dyn DynDigest>)> =
        algorithms.iter().map(|&a| (a, a.hasher())).collect();
    let mut buf = vec![0u8; 256 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        bytes += n as u64;
        for (_, h) in &mut hashers {
            h.update(&buf[..n]);
        }
    }
    Ok(Hashed {
        bytes,
        hashes: hashers
            .into_iter()
            .map(|(a, h)| (a, hex(&h.finalize())))
            .collect(),
    })
}

pub fn hash_bytes(bytes: &[u8], algorithms: &[Algorithm]) -> Hashed {
    hash_reader(bytes, algorithms).expect("reading a slice cannot fail")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash to check against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub algorithm: Algorithm,
    /// Lowercase hex.
    pub hex: String,
}

/// Reads a hash to check against: hex in either case, optionally after
/// its algorithm (`sha256:ab12...`, as Docker writes it). The algorithm
/// comes from the length when it isn't named.
pub fn parse_expected(s: &str) -> Result<Expected, String> {
    let s = s.trim();
    let (named, hex) = match s.split_once(':') {
        Some((label, rest)) => match Algorithm::from_label(label) {
            Some(a) => (Some(a), rest.trim()),
            None => return Err(not_a_hash(s)),
        },
        None => (None, s),
    };
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(not_a_hash(s));
    }
    let by_length = Algorithm::from_hex_len(hex.len());
    let algorithm = match (named, by_length) {
        (Some(a), _) if a.hex_len() == hex.len() => a,
        (Some(a), _) => {
            return Err(format!(
                "{} has {} hex digits, but a {} hash has {}",
                short(s),
                hex.len(),
                a.name(),
                a.hex_len()
            ))
        }
        (None, Some(a)) => a,
        (None, None) => {
            return Err(format!(
                "a hash of {} hex digits isn't one conv knows: MD5 has 32, SHA-1 40, SHA-256 64, SHA-384 96 and SHA-512 128",
                hex.len()
            ))
        }
    };
    Ok(Expected {
        algorithm,
        hex: hex.to_ascii_lowercase(),
    })
}

fn not_a_hash(s: &str) -> String {
    format!(
        "{} isn't a hash: a hash is hex digits (0-9, a-f), like the ones conv hash prints",
        short(s)
    )
}

/// `s` quoted, cut down to something a message can hold.
fn short(s: &str) -> String {
    if s.chars().count() > 40 {
        let head: String = s.chars().take(37).collect();
        format!("\"{head}...\"")
    } else {
        format!("{s:?}")
    }
}

/// One line of a SHA256SUMS-style file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumsLine {
    pub expected: Expected,
    pub file: String,
    /// 1-based, for messages.
    pub line: usize,
}

/// What a sums file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sums {
    pub lines: Vec<SumsLine>,
    /// Lines that are neither a hash nor blank nor a `#` comment.
    pub malformed: Vec<usize>,
}

/// Reads a sums file as `sha256sum` and its relatives write it (`HASH
/// NAME`, `HASH *NAME`), or as BSD tools do (`SHA256 (NAME) = HASH`).
/// Each line's algorithm comes from its hash's length, or the BSD label.
pub fn parse_sums(text: &str) -> Sums {
    let mut sums = Sums {
        lines: Vec::new(),
        malformed: Vec::new(),
    };
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_gnu(line).or_else(|| parse_bsd(trimmed)) {
            Some((expected, file)) => sums.lines.push(SumsLine {
                expected,
                file,
                line: i + 1,
            }),
            None => sums.malformed.push(i + 1),
        }
    }
    sums
}

fn parse_gnu(line: &str) -> Option<(Expected, String)> {
    // A leading backslash says the name has \\ and \n escapes.
    let (escaped, line) = match line.strip_prefix('\\') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (hex, rest) = line.split_once(' ')?;
    let algorithm = Algorithm::from_hex_len(hex.len())?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // Two spaces for text mode, a space and * for binary; one space is
    // what some tools write.
    let name = rest
        .strip_prefix(' ')
        .or_else(|| rest.strip_prefix('*'))
        .unwrap_or(rest);
    if name.is_empty() {
        return None;
    }
    let name = if escaped {
        name.replace("\\n", "\n").replace("\\\\", "\\")
    } else {
        name.to_string()
    };
    Some((
        Expected {
            algorithm,
            hex: hex.to_ascii_lowercase(),
        },
        name,
    ))
}

fn parse_bsd(line: &str) -> Option<(Expected, String)> {
    let (label, rest) = line.split_once(" (")?;
    let algorithm = Algorithm::from_label(label)?;
    let (name, hex) = rest.rsplit_once(") = ")?;
    let hex = hex.trim();
    if hex.len() != algorithm.hex_len() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        Expected {
            algorithm,
            hex: hex.to_ascii_lowercase(),
        },
        name.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hashes_of_hello() {
        let h = hash_bytes(b"hello", &Algorithm::ALL);
        assert_eq!(h.bytes, 5);
        assert_eq!(
            h.get(Algorithm::Md5),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
        assert_eq!(
            h.get(Algorithm::Sha1),
            Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")
        );
        assert_eq!(
            h.get(Algorithm::Sha256),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        assert!(h
            .get(Algorithm::Sha512)
            .unwrap()
            .starts_with("9b71d224bd62f378"));
        assert_eq!(h.get(Algorithm::Sha384).unwrap().len(), 96);
    }

    #[test]
    fn the_empty_input_has_its_well_known_hash() {
        let h = hash_bytes(b"", &[Algorithm::Sha256]);
        assert_eq!(
            h.hashes[0].1,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_large_input_hashes_across_reads() {
        let data = vec![7u8; 1_000_003];
        let once = hash_bytes(&data, &[Algorithm::Sha256]);
        let mut chunked = sha2::Sha256::default();
        sha2::Digest::update(&mut chunked, &data);
        assert_eq!(once.hashes[0].1, hex(&sha2::Digest::finalize(chunked)));
    }

    #[test]
    fn an_expected_hash_names_its_algorithm_by_length() {
        let e = parse_expected("  5D41402ABC4B2A76B9719D911017C592 ").unwrap();
        assert_eq!(e.algorithm, Algorithm::Md5);
        assert_eq!(e.hex, "5d41402abc4b2a76b9719d911017c592");
        let e = parse_expected(&"a".repeat(64)).unwrap();
        assert_eq!(e.algorithm, Algorithm::Sha256);
        let e = parse_expected(&format!("sha512:{}", "b".repeat(128))).unwrap();
        assert_eq!(e.algorithm, Algorithm::Sha512);
    }

    #[test]
    fn a_bad_expected_hash_says_why() {
        assert!(parse_expected("hello")
            .unwrap_err()
            .contains("isn't a hash"));
        assert!(parse_expected("abc123")
            .unwrap_err()
            .contains("6 hex digits isn't one conv knows"));
        let e = parse_expected(&format!("sha1:{}", "a".repeat(64))).unwrap_err();
        assert!(e.contains("SHA-1 hash has 40"), "{e}");
        assert!(parse_expected("SHA256SUMS")
            .unwrap_err()
            .contains("isn't a hash"));
    }

    #[test]
    fn sums_files_in_gnu_and_bsd_form() {
        let text = format!(
            "# release 1.0\n{a}  app.tar.gz\n{b} *app.zip\r\n\nSHA256 (notes.txt) = {c}\nnot a line\n",
            a = "a".repeat(64),
            b = "B".repeat(64),
            c = "c".repeat(64)
        );
        let sums = parse_sums(&text);
        let files: Vec<&str> = sums.lines.iter().map(|l| l.file.as_str()).collect();
        assert_eq!(files, ["app.tar.gz", "app.zip", "notes.txt"]);
        assert_eq!(sums.lines[1].expected.hex, "b".repeat(64));
        assert_eq!(sums.lines[2].line, 5);
        assert_eq!(sums.malformed, [6]);
    }

    #[test]
    fn a_sums_file_may_mix_algorithms_and_escape_names() {
        let text = format!(
            "{}  old.iso\n\\{}  new\\nline.iso\n",
            "d".repeat(32),
            "e".repeat(40)
        );
        let sums = parse_sums(&text);
        assert_eq!(sums.lines[0].expected.algorithm, Algorithm::Md5);
        assert_eq!(sums.lines[1].expected.algorithm, Algorithm::Sha1);
        assert_eq!(sums.lines[1].file, "new\nline.iso");
    }

    #[test]
    fn labels_read_loosely() {
        assert_eq!(Algorithm::from_label("SHA-256"), Some(Algorithm::Sha256));
        assert_eq!(Algorithm::from_label("md5"), Some(Algorithm::Md5));
        assert_eq!(Algorithm::from_label("crc32"), None);
    }
}
