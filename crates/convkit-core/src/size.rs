//! The `--max-size` value: how many bytes, which unit family the user wrote
//! it in, and the spelling to reuse in a file name.
//!
//! Decimal units (`kb`, `mb`, `gb`) are the default reading of "10 MB"
//! because they are the smaller one: a file sized for `10mb` (10,000,000
//! bytes) also passes a limit enforced as 10 MiB (10,485,760). Parsing is
//! integer arithmetic on the digit strings, never floating point, so
//! `1.5gb` is exactly 1,500,000,000 bytes.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitFamily {
    Decimal,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxSize {
    pub bytes: u64,
    /// Lowercased, otherwise as typed (`10mb`, `1.5gb`, `10mib`): reused
    /// verbatim in the `clip-10mb.mp4` file name and in a suggested command.
    pub spelling: String,
    pub family: UnitFamily,
}

/// Suffix, bytes per unit, family, display label; smallest first within
/// each family. No suffix here ends another one, so the first suffix match
/// is the only one. A `static`, not a `const`, so `units_of` can hand out
/// `'static` references into it.
static UNITS: [(&str, u64, UnitFamily, &str); 6] = [
    ("kib", 1 << 10, UnitFamily::Binary, "KiB"),
    ("mib", 1 << 20, UnitFamily::Binary, "MiB"),
    ("gib", 1 << 30, UnitFamily::Binary, "GiB"),
    ("kb", 1_000, UnitFamily::Decimal, "KB"),
    ("mb", 1_000_000, UnitFamily::Decimal, "MB"),
    ("gb", 1_000_000_000, UnitFamily::Decimal, "GB"),
];

fn units_of(
    family: UnitFamily,
) -> impl DoubleEndedIterator<Item = &'static (&'static str, u64, UnitFamily, &'static str)> {
    UNITS.iter().filter(move |u| u.2 == family)
}

/// Parses `<number><unit>`: digits, an optional fraction of at most three
/// digits, and one of `kb mb gb kib mib gib`, case-insensitive, no space.
pub fn parse(text: &str) -> Result<MaxSize, String> {
    let lower = text.trim().to_ascii_lowercase();
    let Some(&(suffix, scale, family, _)) = UNITS
        .iter()
        .find(|(s, ..)| lower.len() > s.len() && lower.ends_with(s))
    else {
        return Err(format!(
            "invalid size '{text}'; add a unit, e.g. 10mb or 10mib"
        ));
    };
    let number = &lower[..lower.len() - suffix.len()];
    let (whole, frac) = number.split_once('.').unwrap_or((number, ""));
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || !all_digits(whole)
        || !all_digits(frac)
        || frac.len() > 3
        || (number.contains('.') && frac.is_empty())
    {
        return Err(format!(
            "invalid size '{text}'; expected a number and a unit, e.g. 10mb or 1.5gb"
        ));
    }
    let too_large = || format!("invalid size '{text}'; it is too large");
    let scale = u128::from(scale);
    let whole: u128 = whole.parse().map_err(|_| too_large())?;
    let frac_value: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| too_large())?
    };
    let bytes = whole
        .checked_mul(scale)
        .and_then(|b| b.checked_add(frac_value * scale / 10u128.pow(frac.len() as u32)))
        .ok_or_else(too_large)?;
    if bytes == 0 {
        return Err(format!(
            "invalid size '{text}'; it must be larger than zero"
        ));
    }
    let bytes = u64::try_from(bytes).map_err(|_| too_large())?;
    Ok(MaxSize {
        bytes,
        spelling: lower,
        family,
    })
}

impl MaxSize {
    /// `10mb` reads as `10 MB`, `1.5gib` as `1.5 GiB`.
    pub fn label(&self) -> String {
        let (suffix, _, _, label) = UNITS
            .iter()
            .find(|(s, ..)| self.spelling.ends_with(s))
            .expect("a parsed MaxSize always ends with a known unit");
        format!(
            "{} {label}",
            &self.spelling[..self.spelling.len() - suffix.len()]
        )
    }
}

/// A byte count in `family`'s units, **floored** to two decimals: a file
/// one byte under 10 MB shows as `9.99 MB`, never as `10.00 MB`.
pub fn display(bytes: u64, family: UnitFamily) -> String {
    let Some(&(_, scale, _, label)) = units_of(family).rev().find(|u| bytes >= u.1) else {
        return format!("{bytes} B");
    };
    let hundredths = u128::from(bytes) * 100 / u128::from(scale);
    format!("{}.{:02} {label}", hundredths / 100, hundredths % 100)
}

/// The smallest clean size at or above `bytes`: two significant figures,
/// the second raised to a multiple of 5 (38.2 MB -> 40mb, 7.3 -> 7.5mb,
/// 142 -> 150mb), in the largest unit of `family` the value reaches.
pub fn round_up(bytes: u64, family: UnitFamily) -> MaxSize {
    let &(suffix, scale, ..) = units_of(family)
        .rev()
        .find(|u| bytes >= u.1)
        .unwrap_or_else(|| units_of(family).next().expect("every family has units"));
    // Thousandths of the unit, rounded up, so everything below is integer.
    let milli = (u128::from(bytes) * 1000).div_ceil(u128::from(scale));
    let digits = milli.to_string().len() as u32;
    let step = 10u128.pow(digits.saturating_sub(2)) * 5;
    let rounded = milli.div_ceil(step) * step;
    // Rounding can carry into the next unit (999.9 MB -> 1000 MB = 1 GB).
    if let Some(&(_, next, ..)) = units_of(family).find(|u| u.1 > scale) {
        let rounded_bytes = rounded * u128::from(scale) / 1000;
        if rounded_bytes >= u128::from(next) {
            return round_up(rounded_bytes as u64, family);
        }
    }
    let (int, frac) = (rounded / 1000, rounded % 1000);
    let number = if frac == 0 {
        int.to_string()
    } else {
        format!("{int}.{frac:03}").trim_end_matches('0').to_string()
    };
    parse(&format!("{number}{suffix}")).expect("round_up always builds a valid size")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> u64 {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}")).bytes
    }

    #[test]
    fn every_unit_parses_to_the_exact_byte_count() {
        assert_eq!(bytes("500kb"), 500_000);
        assert_eq!(bytes("10mb"), 10_000_000);
        assert_eq!(bytes("1.5gb"), 1_500_000_000);
        assert_eq!(bytes("2kib"), 2_048);
        assert_eq!(bytes("10mib"), 10_485_760);
        assert_eq!(bytes("1gib"), 1_073_741_824);
        assert_eq!(bytes("1.5kib"), 1_536);
        assert_eq!(bytes("0.001gb"), 1_000_000);
    }

    #[test]
    fn case_is_ignored_and_the_spelling_is_normalised() {
        let m = parse("10MB").unwrap();
        assert_eq!(m.bytes, 10_000_000);
        assert_eq!(m.spelling, "10mb");
        assert_eq!(m.family, UnitFamily::Decimal);
        assert_eq!(parse("10MiB").unwrap().family, UnitFamily::Binary);
    }

    #[test]
    fn malformed_sizes_are_refused_with_the_fix() {
        for (input, needle) in [
            ("10", "add a unit"),
            ("mb", "add a unit"),
            ("10 mb", "expected a number"),
            ("1.5555gb", "expected a number"),
            ("1.mb", "expected a number"),
            ("-1mb", "expected a number"),
            ("0mb", "larger than zero"),
            ("99999999999999999999999999999gb", "too large"),
        ] {
            let e = parse(input).unwrap_err();
            assert!(e.contains(needle), "{input}: {e}");
            assert!(e.starts_with("invalid size"), "{input}: {e}");
        }
    }

    #[test]
    fn a_label_reads_the_way_a_person_writes_it() {
        assert_eq!(parse("10mb").unwrap().label(), "10 MB");
        assert_eq!(parse("1.5gib").unwrap().label(), "1.5 GiB");
    }

    /// Floored, never rounded: a file under the target must never print as
    /// the target or above it.
    #[test]
    fn display_floors_to_two_decimals() {
        assert_eq!(display(9_999_999, UnitFamily::Decimal), "9.99 MB");
        assert_eq!(display(10_000_000, UnitFamily::Decimal), "10.00 MB");
        assert_eq!(display(1_500, UnitFamily::Decimal), "1.50 KB");
        assert_eq!(display(999, UnitFamily::Decimal), "999 B");
        assert_eq!(display(10_485_760, UnitFamily::Binary), "10.00 MiB");
    }

    #[test]
    fn round_up_lands_on_a_clean_number_at_or_above_the_input() {
        assert_eq!(round_up(38_200_000, UnitFamily::Decimal).spelling, "40mb");
        assert_eq!(round_up(7_300_000, UnitFamily::Decimal).spelling, "7.5mb");
        assert_eq!(round_up(142_000_000, UnitFamily::Decimal).spelling, "150mb");
        assert_eq!(round_up(999_900_000, UnitFamily::Decimal).spelling, "1gb");
        assert_eq!(round_up(3 << 20, UnitFamily::Binary).spelling, "3mib");
        for b in [
            1u64,
            999,
            1_001,
            123_456,
            9_999_999,
            10_000_001,
            4_321_987_654,
        ] {
            for fam in [UnitFamily::Decimal, UnitFamily::Binary] {
                let r = round_up(b, fam);
                assert!(r.bytes >= b, "{b} {fam:?} -> {}", r.spelling);
                assert_eq!(parse(&r.spelling).unwrap(), r);
            }
        }
    }
}
