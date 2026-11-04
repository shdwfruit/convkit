//! `conv uuid`: new UUIDs, and what an existing one says about itself.

use jiff::Timestamp;
use uuid::{Uuid, Variant};

/// A random (version 4) UUID.
pub fn new_v4() -> String {
    Uuid::new_v4().hyphenated().to_string()
}

/// A time-ordered (version 7) UUID: the current Unix time in milliseconds,
/// then random bits. These sort in the order they were made, which keeps
/// database indexes compact.
pub fn new_v7() -> String {
    Uuid::now_v7().hyphenated().to_string()
}

/// What a UUID says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    /// Hyphenated and lowercase, however it was written.
    pub uuid: String,
    /// 1 to 8, or `None` for the nil and max UUIDs and other variants.
    pub version: Option<u8>,
    /// What the version means: "random", "Unix time and random", ...
    pub kind: &'static str,
    pub variant: &'static str,
    /// When it was made, for versions 1, 6 and 7.
    pub timestamp: Option<Timestamp>,
    /// The clock sequence of a version 1 or 6 UUID.
    pub clock_sequence: Option<u16>,
    /// The node of a version 1 or 6 UUID: a MAC address, unless its
    /// multicast bit says it was made up.
    pub node: Option<String>,
    pub node_is_random: bool,
}

/// Reads a UUID in any of the usual forms (hyphenated, 32 hex digits,
/// `{braced}`, `urn:uuid:`) and says what it is.
pub fn inspect(s: &str) -> Result<Inspected, String> {
    let s = s.trim();
    let uuid = parse(s)?;
    let bytes = uuid.as_bytes();
    let rfc = uuid.get_variant() == Variant::RFC4122;
    let version = if uuid.is_nil() || uuid.is_max() || !rfc {
        None
    } else {
        Some(uuid.get_version_num() as u8)
    };
    let kind = if uuid.is_nil() {
        "the nil UUID (all zeros)"
    } else if uuid.is_max() {
        "the max UUID (all ones)"
    } else {
        match version {
            Some(1) => "time and node (MAC address)",
            Some(2) => "DCE security",
            Some(3) => "name-based, MD5",
            Some(4) => "random",
            Some(5) => "name-based, SHA-1",
            Some(6) => "time and node, reordered to sort by time",
            Some(7) => "Unix time and random, sorts by time",
            Some(8) => "custom",
            Some(_) => "a version no standard defines",
            None => "not an RFC 9562 UUID",
        }
    };
    let variant = if uuid.is_nil() || uuid.is_max() {
        "none (a special value)"
    } else {
        match uuid.get_variant() {
            Variant::NCS => "NCS (reserved, from before the standard)",
            Variant::RFC4122 => "RFC 9562 (the standard one)",
            Variant::Microsoft => "Microsoft (reserved, old COM GUIDs)",
            _ => "reserved for the future",
        }
    };
    let gregorian = matches!(version, Some(1 | 6)).then(|| uuid.get_timestamp());
    let (timestamp, clock_sequence) = match version {
        Some(1 | 6) => match gregorian.flatten() {
            Some(ts) => {
                let (ticks, clock) = ts.to_gregorian();
                (from_gregorian(ticks), Some(clock))
            }
            None => (None, None),
        },
        Some(7) => {
            let ms = u64::from_be_bytes([
                0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
            ]);
            (Timestamp::from_millisecond(ms as i64).ok(), None)
        }
        _ => (None, None),
    };
    let node_bytes = matches!(version, Some(1 | 6))
        .then(|| uuid.get_node_id())
        .flatten();
    let node = node_bytes.map(|n| {
        n.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    });
    // The multicast bit of the first byte is set in a node that isn't a
    // real network card's address (RFC 9562 section 6.10).
    let node_is_random = node_bytes.is_some_and(|n| n[0] & 1 == 1);
    Ok(Inspected {
        uuid: uuid.hyphenated().to_string(),
        version,
        kind,
        variant,
        timestamp,
        clock_sequence,
        node,
        node_is_random,
    })
}

fn parse(s: &str) -> Result<Uuid, String> {
    let bare = s
        .strip_prefix("urn:uuid:")
        .or_else(|| s.strip_prefix("URN:UUID:"))
        .unwrap_or(s);
    let bare = bare
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(bare);
    if let Some(c) = bare.chars().find(|c| !c.is_ascii_hexdigit() && *c != '-') {
        return Err(format!(
            "{s:?} isn't a UUID: {c:?} isn't a hex digit; a UUID is 32 hex digits, usually as 8-4-4-4-12"
        ));
    }
    let digits = bare.chars().filter(|c| *c != '-').count();
    if digits != 32 {
        return Err(format!(
            "{s:?} isn't a UUID: it has {digits} hex digits, and a UUID has 32"
        ));
    }
    Uuid::try_parse(bare)
        .map_err(|_| format!("{s:?} isn't a UUID: its hyphens aren't where they go, as 8-4-4-4-12"))
}

/// 100-nanosecond ticks since the Gregorian calendar began (15 October
/// 1582), as a Unix timestamp. Done here rather than by the uuid crate,
/// whose Unix conversion is unsigned and wraps for dates before 1970.
fn from_gregorian(ticks: u64) -> Option<Timestamp> {
    const GREGORIAN_TO_UNIX_SECONDS: i128 = 12_219_292_800;
    let nanos = ticks as i128 * 100 - GREGORIAN_TO_UNIX_SECONDS * 1_000_000_000;
    Timestamp::from_nanosecond(nanos).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ones_have_the_version_asked_for() {
        let v4 = inspect(&new_v4()).unwrap();
        assert_eq!(v4.version, Some(4));
        assert_eq!(v4.timestamp, None);
        let v7 = inspect(&new_v7()).unwrap();
        assert_eq!(v7.version, Some(7));
        let age = Timestamp::now().as_millisecond() - v7.timestamp.unwrap().as_millisecond();
        assert!((0..10_000).contains(&age), "{age} ms");
    }

    #[test]
    fn v7_ones_sort_in_the_order_they_were_made() {
        let ids: Vec<String> = (0..50).map(|_| new_v7()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn a_v7_gives_its_time() {
        // 2022-02-22 19:22:22 UTC, from RFC 9562's own example.
        let i = inspect("017F22E2-79B0-7CC3-98C4-DC0C0C07398F").unwrap();
        assert_eq!(i.uuid, "017f22e2-79b0-7cc3-98c4-dc0c0c07398f");
        assert_eq!(i.version, Some(7));
        assert_eq!(i.timestamp.unwrap().to_string(), "2022-02-22T19:22:22Z");
    }

    #[test]
    fn a_v1_gives_its_time_clock_and_node() {
        // RFC 9562's version 1 example.
        let i = inspect("C232AB00-9414-11EC-B3C8-9F6BDECED846").unwrap();
        assert_eq!(i.version, Some(1));
        assert_eq!(i.timestamp.unwrap().to_string(), "2022-02-22T19:22:22Z");
        assert_eq!(i.clock_sequence, Some(0x33c8));
        assert_eq!(i.node.as_deref(), Some("9f:6b:de:ce:d8:46"));
        assert!(i.node_is_random);
    }

    #[test]
    fn a_v1_from_before_1970_does_not_wrap() {
        // The Gregorian epoch itself: all-zero time fields.
        let i = inspect("00000000-0000-1000-8000-000000000000").unwrap();
        assert_eq!(i.timestamp.unwrap().to_string(), "1582-10-15T00:00:00Z");
    }

    #[test]
    fn the_usual_forms_all_read() {
        let want = "f81d4fae-7dec-11d0-a765-00a0c91e6bf6";
        for form in [
            "f81d4fae-7dec-11d0-a765-00a0c91e6bf6",
            "F81D4FAE7DEC11D0A76500A0C91E6BF6",
            "{f81d4fae-7dec-11d0-a765-00a0c91e6bf6}",
            "urn:uuid:f81d4fae-7dec-11d0-a765-00a0c91e6bf6",
        ] {
            assert_eq!(inspect(form).unwrap().uuid, want, "{form}");
        }
    }

    #[test]
    fn nil_and_max_are_named() {
        let nil = inspect("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(nil.version, None);
        assert!(nil.kind.contains("nil"));
        let max = inspect("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        assert!(max.kind.contains("max"));
    }

    #[test]
    fn a_bad_one_says_why() {
        assert!(inspect("not-a-uuid")
            .unwrap_err()
            .contains("'n' isn't a hex digit"));
        assert!(inspect("1234").unwrap_err().contains("4 hex digits"));
        assert!(inspect("f81d4fae7-dec-11d0-a765-00a0c91e6bf6")
            .unwrap_err()
            .contains("hyphens"));
    }

    #[test]
    fn a_microsoft_guid_is_another_variant() {
        let i = inspect("00000000-0000-4000-c000-000000000001").unwrap();
        assert_eq!(i.version, None);
        assert!(i.variant.contains("Microsoft"));
    }
}
