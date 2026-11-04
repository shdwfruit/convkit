//! `conv time`: Unix timestamps to dates and back.
//!
//! A number is a Unix timestamp whose unit comes from its size (seconds,
//! milliseconds, microseconds or nanoseconds), and the reading says which
//! it took. A date without a zone is read in the local zone, and says that
//! too, along with what daylight saving did to it.

use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};
use jiff::{Span, Timestamp, Unit as JUnit, Zoned};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

impl Unit {
    pub fn name(self) -> &'static str {
        match self {
            Unit::Seconds => "seconds",
            Unit::Milliseconds => "milliseconds",
            Unit::Microseconds => "microseconds",
            Unit::Nanoseconds => "nanoseconds",
        }
    }

    pub fn parse(s: &str) -> Option<Unit> {
        match s.to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(Unit::Seconds),
            "ms" | "milli" | "millis" | "millisecond" | "milliseconds" => Some(Unit::Milliseconds),
            "us" | "µs" | "micro" | "micros" | "microsecond" | "microseconds" => {
                Some(Unit::Microseconds)
            }
            "ns" | "nano" | "nanos" | "nanosecond" | "nanoseconds" => Some(Unit::Nanoseconds),
            _ => None,
        }
    }

    fn nanos(self) -> i128 {
        match self {
            Unit::Seconds => 1_000_000_000,
            Unit::Milliseconds => 1_000_000,
            Unit::Microseconds => 1_000,
            Unit::Nanoseconds => 1,
        }
    }
}

/// How the input was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadAs {
    Now,
    /// A Unix timestamp; `guessed` when its unit came from its size.
    Unix {
        unit: Unit,
        guessed: bool,
    },
    /// A date and time; `zone` is the zone it was read in when it named
    /// none.
    Date {
        assumed_zone: Option<String>,
    },
    /// "3 days ago", "in 2 hours", "yesterday".
    Relative,
}

/// What was read: an instant, how, and what to know about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub at: Timestamp,
    pub read_as: ReadAs,
    pub notes: Vec<String>,
}

/// Reads `input` as a time. `unit` overrides the guess for a number;
/// `local` is the zone a date without one is read in.
pub fn read(
    input: &str,
    unit: Option<Unit>,
    local: &TimeZone,
    now: Timestamp,
) -> Result<Reading, String> {
    let s = input.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("now") {
        return Ok(Reading {
            at: now,
            read_as: ReadAs::Now,
            notes: vec![],
        });
    }
    if let Some(r) = read_number(s, unit)? {
        return Ok(r);
    }
    if unit.is_some() {
        return Err(format!(
            "--unit is for a Unix timestamp, and {s:?} isn't a number"
        ));
    }
    if let Some(r) = read_relative(s, local, now)? {
        return Ok(r);
    }
    read_date(s, local)
}

/// The unit a timestamp of this size most likely is: seconds up to the
/// year 5138, then milliseconds, microseconds and nanoseconds, each a
/// thousand times bigger.
pub fn guess_unit(whole: u128) -> Unit {
    if whole < 100_000_000_000 {
        Unit::Seconds
    } else if whole < 100_000_000_000_000 {
        Unit::Milliseconds
    } else if whole < 100_000_000_000_000_000 {
        Unit::Microseconds
    } else {
        Unit::Nanoseconds
    }
}

fn read_number(s: &str, unit: Option<Unit>) -> Result<Option<Reading>, String> {
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let is_digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if !is_digits(whole) || !(frac.is_empty() || is_digits(frac)) {
        return Ok(None);
    }
    let whole_n: u128 = whole
        .parse()
        .map_err(|_| format!("{s} is far too big to be a time"))?;
    let (unit, guessed) = match unit {
        Some(u) => (u, false),
        None => (guess_unit(whole_n), true),
    };
    // Exact to the nanosecond: the fraction is taken as digits, never
    // through a float.
    let scale = unit.nanos();
    let frac_digits = frac.len().min(9);
    let frac_n: i128 = if frac.is_empty() {
        0
    } else {
        frac[..frac_digits].parse::<i128>().unwrap_or(0) * scale / 10i128.pow(frac_digits as u32)
    };
    let magnitude = (whole_n as i128)
        .checked_mul(scale)
        .and_then(|n| n.checked_add(frac_n))
        .ok_or_else(|| format!("{s} is far too big to be a time"))?;
    let nanos = if negative { -magnitude } else { magnitude };
    let at = Timestamp::from_nanosecond(nanos).map_err(|_| {
        format!(
            "{s} {} is outside the years -9999 to 9999 that conv can show",
            unit.name()
        )
    })?;
    let mut notes = Vec::new();
    if guessed && (1000..=9999).contains(&whole_n) && frac.is_empty() && !negative {
        notes.push(format!(
            "{s} is read as seconds since 1970; for the year, write {s}-01-01"
        ));
    }
    Ok(Some(Reading {
        at,
        read_as: ReadAs::Unix { unit, guessed },
        notes,
    }))
}

fn read_relative(s: &str, local: &TimeZone, now: Timestamp) -> Result<Option<Reading>, String> {
    let lower = s.to_ascii_lowercase();
    let today = now.to_zoned(local.clone()).date();
    let day = match lower.as_str() {
        "today" => Some(today),
        "yesterday" => today.yesterday().ok(),
        "tomorrow" => today.tomorrow().ok(),
        _ => None,
    };
    if let Some(day) = day {
        let at = in_zone(day.to_datetime(jiff::civil::Time::midnight()), local)?;
        return Ok(Some(Reading {
            at: at.0,
            read_as: ReadAs::Relative,
            notes: at.1,
        }));
    }
    let (text, forward) = match lower.strip_prefix("in ") {
        Some(rest) => (rest.trim(), true),
        None => (lower.as_str(), false),
    };
    if !forward && !text.ends_with(" ago") {
        return Ok(None);
    }
    let Ok(span) = text.parse::<Span>() else {
        return Ok(None);
    };
    // In UTC, so "3 days ago" is 72 hours ago across a clock change too,
    // as `relative` describes it.
    let from = now.to_zoned(TimeZone::UTC);
    let at = from
        .checked_add(span)
        .map_err(|_| format!("{s:?} is outside the years conv can show"))?;
    Ok(Some(Reading {
        at: at.timestamp(),
        read_as: ReadAs::Relative,
        notes: vec![],
    }))
}

fn read_date(s: &str, local: &TimeZone) -> Result<Reading, String> {
    // A complete instant: an offset or Z, or a zone in brackets.
    if let Ok(z) = s.parse::<Zoned>() {
        return Ok(dated(z.timestamp(), vec![]));
    }
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Ok(dated(ts, vec![]));
    }
    if let Ok(z) = jiff::fmt::rfc2822::parse(s) {
        return Ok(dated(z.timestamp(), vec![]));
    }
    // The zone may be written last, as a name or an offset.
    let (body, zone) = split_zone(s)?;
    let (tz, assumed) = match zone {
        Some(tz) => (tz, false),
        None => (local.clone(), true),
    };
    let Some(dt) = civil(&body)? else {
        check_calendar(&body)?;
        return Err(format!(
            "conv can't read {s:?} as a time; it reads forms like 2024-02-29 14:30, 29 Feb 2024, Tue, 1 Jul 2003 10:52:37 +0200, 1709217000 or 3 days ago"
        ));
    };
    let (at, notes) = in_zone(dt, &tz)?;
    let assumed_zone = assumed.then(|| zone_name(&tz));
    Ok(Reading {
        at,
        read_as: ReadAs::Date { assumed_zone },
        notes,
    })
}

fn dated(at: Timestamp, notes: Vec<String>) -> Reading {
    Reading {
        at,
        read_as: ReadAs::Date { assumed_zone: None },
        notes,
    }
}

/// Splits a zone off the end: `UTC`, `GMT`, `Z`, an IANA name, or an
/// offset like `+02:00`, or one just before the year as date(1) writes it
/// (`Tue Nov 3 16:00:00 EST 2026`). An abbreviation like `EST` is refused,
/// since several zones share most of them.
fn split_zone(s: &str) -> Result<(String, Option<TimeZone>), String> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 2 {
        return Ok((s.to_string(), None));
    }
    let last = tokens.len() - 1;
    let year_last = tokens[last].len() == 4 && tokens[last].bytes().all(|b| b.is_ascii_digit());
    let before_year = year_last
        && tokens.len() >= 3
        && is_abbreviation(tokens[last - 1])
        && !is_month_or_day(tokens[last - 1]);
    let at = if before_year { last - 1 } else { last };
    let Some(tz) = zone_of(tokens[at])? else {
        return Ok((s.to_string(), None));
    };
    let rest: Vec<&str> = tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != at)
        .map(|(_, t)| *t)
        .collect();
    Ok((rest.join(" "), Some(tz)))
}

/// The zone a token names, `None` when it names none, or an error for an
/// abbreviation that could be several.
fn zone_of(token: &str) -> Result<Option<TimeZone>, String> {
    if ["utc", "gmt", "z"].contains(&token.to_ascii_lowercase().as_str()) {
        return Ok(Some(TimeZone::UTC));
    }
    if token.contains('/') && !token.starts_with(|c: char| c.is_ascii_digit()) {
        if let Ok(tz) = TimeZone::get(token) {
            return Ok(Some(tz));
        }
    }
    if let Some(offset) = parse_offset(token) {
        return Ok(Some(TimeZone::fixed(offset)));
    }
    if token.len() >= 3 && is_abbreviation(token) && !is_month_or_day(token) {
        return Err(format!(
            "{token} could be more than one time zone (CST is US Central or China), so conv won't guess; write an offset like -05:00 or a name like America/Chicago"
        ));
    }
    Ok(None)
}

fn is_abbreviation(s: &str) -> bool {
    (1..=5).contains(&s.len()) && s.chars().all(|c| c.is_ascii_uppercase())
}

fn is_month_or_day(s: &str) -> bool {
    const NAMES: [&str; 19] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "SEPT", "OCT", "NOV", "DEC",
        "MON", "TUE", "WED", "THU", "FRI", "SAT",
    ];
    NAMES.contains(&s) || s == "SUN"
}

/// `+02:00`, `+0200`, `+02`, `-0530`.
fn parse_offset(s: &str) -> Option<Offset> {
    let sign = match s.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits: String = s[1..].chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) || ![2, 4].contains(&digits.len()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits.get(2..).map_or(Some(0), |m| m.parse().ok())?;
    if hours > 25 || minutes > 59 {
        return None;
    }
    Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}

/// The forms that, beyond ISO 8601, people paste most: the date first in
/// words or with slashes, and an optional time.
const DATE_FORMS: &[&str] = &[
    "%Y/%m/%d",
    "%d %B %Y",
    "%d %b %Y",
    "%B %d, %Y",
    "%b %d, %Y",
    "%B %d %Y",
    "%b %d %Y",
    "%a %b %d %Y",
    "%a, %d %b %Y",
    "%A, %B %d, %Y",
    "%a, %b %d, %Y",
];

const TIME_FORMS: &[&str] = &[
    "",
    " %H:%M",
    " %H:%M:%S",
    " %I:%M %p",
    " %I:%M:%S %p",
    " %I %p",
    " %I%p",
    " %I:%M%p",
    ", %H:%M",
    ", %I:%M %p",
    " at %H:%M",
    " at %I:%M %p",
];

/// A civil date and time, without a zone.
fn civil(s: &str) -> Result<Option<DateTime>, String> {
    if let Ok(dt) = s.parse::<DateTime>() {
        return Ok(Some(dt));
    }
    if let Ok(d) = s.parse::<Date>() {
        return Ok(Some(d.to_datetime(jiff::civil::Time::midnight())));
    }
    if let Some(dt) = numeric_date(s)? {
        return Ok(Some(dt));
    }
    // ctime and date(1) without the zone: "Tue Nov  3 16:00:00 2026".
    let squeezed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    for form in ["%a %b %d %H:%M:%S %Y", "%a %d %b %Y %H:%M:%S"] {
        if let Ok(dt) = jiff::fmt::strtime::parse(form, &squeezed).and_then(|b| b.to_datetime()) {
            return Ok(Some(dt));
        }
    }
    for date in DATE_FORMS {
        for time in TIME_FORMS {
            let form = format!("{date}{time}");
            let Ok(parsed) = jiff::fmt::strtime::parse(&form, &squeezed) else {
                continue;
            };
            let dt = if time.is_empty() {
                parsed
                    .to_date()
                    .map(|d| d.to_datetime(jiff::civil::Time::midnight()))
            } else {
                parsed.to_datetime()
            };
            if let Ok(dt) = dt {
                return Ok(Some(dt));
            }
        }
    }
    Ok(None)
}

/// `25/12/2024`, `12/25/2024`, `1.2.2024`, with an optional time. Dots
/// put the day first, as everywhere they are used. Slashes put the month
/// first in the US and the day first elsewhere, so a date whose first two
/// numbers both fit as a month is refused rather than guessed.
fn numeric_date(s: &str) -> Result<Option<DateTime>, String> {
    let (date, time) = match s.split_once(' ') {
        Some((d, t)) => (d, Some(t.trim())),
        None => (s, None),
    };
    let sep = if date.contains('/') {
        '/'
    } else if date.contains('.') {
        '.'
    } else {
        return Ok(None);
    };
    let parts: Vec<&str> = date.split(sep).collect();
    let [a, b, y] = parts.as_slice() else {
        return Ok(None);
    };
    let num = |t: &str| -> Option<i16> {
        (!t.is_empty() && t.len() <= 2 && t.bytes().all(|c| c.is_ascii_digit()))
            .then(|| t.parse().ok())
            .flatten()
    };
    let (Some(a), Some(b)) = (num(a), num(b)) else {
        return Ok(None);
    };
    if y.len() != 4 || !y.bytes().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }
    let year: i16 = y.parse().map_err(|_| "bad year".to_string())?;
    let (month, day) = if sep == '.' || a > 12 {
        (b, a)
    } else if b > 12 || a == b {
        (a, b)
    } else {
        return Err(format!(
            "{date} could be {} or {}, so conv won't guess; write it as {} or {}",
            spoken(year, a, b),
            spoken(year, b, a),
            iso(year, a, b),
            iso(year, b, a)
        ));
    };
    let d = valid_date(year, month, day)?;
    let t = match time {
        None => jiff::civil::Time::midnight(),
        Some(t) => {
            let mut found = None;
            for form in [
                "%H:%M",
                "%H:%M:%S",
                "%I:%M %p",
                "%I:%M:%S %p",
                "%I %p",
                "%I%p",
            ] {
                if let Ok(time) = jiff::fmt::strtime::parse(form, t).and_then(|p| p.to_time()) {
                    found = Some(time);
                    break;
                }
            }
            match found {
                Some(time) => time,
                None => return Ok(None),
            }
        }
    };
    Ok(Some(d.to_datetime(t)))
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn spoken(year: i16, month: i16, day: i16) -> String {
    let _ = year;
    format!("{day} {}", MONTHS[(month - 1) as usize])
}

fn iso(year: i16, month: i16, day: i16) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// A date, or why it isn't one, in words.
fn valid_date(year: i16, month: i16, day: i16) -> Result<Date, String> {
    if !(1..=12).contains(&month) {
        return Err(format!(
            "{} isn't a date: there is no month {month}",
            iso(year, month, day)
        ));
    }
    match Date::new(year, month as i8, day as i8) {
        Ok(d) => Ok(d),
        Err(_) => {
            let days = Date::new(year, month as i8, 1)
                .map(|d| d.days_in_month())
                .unwrap_or(31);
            let why = if month == 2 && day == 29 {
                format!("{year} isn't a leap year, so February has 28 days")
            } else {
                format!("{} {year} has {days} days", MONTHS[(month - 1) as usize])
            };
            Err(format!("{} isn't a date: {why}", iso(year, month, day)))
        }
    }
}

/// When an ISO-looking date fails, says why in words: 2023-02-29 isn't a
/// leap day, April has no 31st.
fn check_calendar(s: &str) -> Result<(), String> {
    let head = s.get(..10).unwrap_or("");
    let b = head.as_bytes();
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' {
        let (Ok(y), Ok(m), Ok(d)) = (
            head[..4].parse::<i16>(),
            head[5..7].parse::<i16>(),
            head[8..10].parse::<i16>(),
        ) else {
            return Ok(());
        };
        valid_date(y, m, d)?;
    }
    Ok(())
}

/// A civil time in `tz`, and what daylight saving did to it: an hour that
/// was skipped is read as the time after the gap, and one that happened
/// twice as the first of the two.
fn in_zone(dt: DateTime, tz: &TimeZone) -> Result<(Timestamp, Vec<String>), String> {
    let ambiguous = tz.to_ambiguous_zoned(dt);
    let offset = ambiguous.offset();
    let zoned = ambiguous
        .compatible()
        .map_err(|_| "that time is outside the years conv can show".to_string())?;
    let name = zone_name(tz);
    let when = dt.strftime("%H:%M on %Y-%m-%d");
    let note = match offset {
        AmbiguousOffset::Unambiguous { .. } => None,
        AmbiguousOffset::Gap { .. } => Some(format!(
            "{when} never happened in {name}: the clocks went forward past it; read as {}",
            zoned.strftime("%H:%M %Z")
        )),
        AmbiguousOffset::Fold { before, after } => Some(format!(
            "{when} happened twice in {name}, as the clocks went back; read as the first, {} ({}); the second is {}",
            zoned.strftime("%H:%M %Z"),
            before,
            after
        )),
    };
    Ok((zoned.timestamp(), note.into_iter().collect()))
}

/// The zone's IANA name, or its offset when it has none.
pub fn zone_name(tz: &TimeZone) -> String {
    match tz.iana_name() {
        Some(n) => n.to_string(),
        None => Timestamp::now().to_zoned(tz.clone()).offset().to_string(),
    }
}

/// One instant as shown in one zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub zone: String,
    /// "Tue 2026-11-03 16:00:00 EST".
    pub human: String,
    /// "-05:00".
    pub offset: String,
    /// RFC 3339: "2026-11-03T16:00:00-05:00".
    pub rfc3339: String,
}

pub fn show(at: Timestamp, tz: &TimeZone) -> Shown {
    let z = at.to_zoned(tz.clone());
    Shown {
        zone: zone_name(tz),
        human: z.strftime("%a %Y-%m-%d %H:%M:%S%.f %Z").to_string(),
        offset: z.strftime("%:z").to_string(),
        rfc3339: z.strftime("%Y-%m-%dT%H:%M:%S%.f%:z").to_string(),
    }
}

/// The Unix time in seconds, with a fraction only when there is one, and
/// in whole milliseconds (rounded down, so before 1970 too).
pub fn unix(at: Timestamp) -> (String, i64) {
    let ns = at.as_nanosecond();
    let sign = if ns < 0 { "-" } else { "" };
    let abs = ns.unsigned_abs();
    let (secs, frac) = (abs / 1_000_000_000, abs % 1_000_000_000);
    let seconds = if frac == 0 {
        format!("{sign}{secs}")
    } else {
        let f = format!("{frac:09}");
        format!("{sign}{secs}.{}", f.trim_end_matches('0'))
    };
    (seconds, ns.div_euclid(1_000_000) as i64)
}

/// "3 days ago", "in 2 hours 5 minutes", "now": the two largest units of
/// the time between `at` and `now`. Counted in UTC, so it is the time that
/// really passed: a day across a clock change is 23 or 25 hours.
pub fn relative(at: Timestamp, now: Timestamp) -> String {
    let diff = at.as_nanosecond() - now.as_nanosecond();
    if diff.abs() < 1_000_000_000 {
        return "now".to_string();
    }
    // Forward from the earlier one, so months and years land on dates
    // that exist.
    let (early, late) = if diff < 0 { (at, now) } else { (now, at) };
    let early = early.to_zoned(TimeZone::UTC);
    let late = late.to_zoned(TimeZone::UTC);
    let Ok(span) = early.until((JUnit::Year, &late)) else {
        return String::new();
    };
    let units = [
        (span.get_years() as i64, "year"),
        (span.get_months() as i64, "month"),
        (span.get_weeks() as i64 * 7 + span.get_days() as i64, "day"),
        (span.get_hours() as i64, "hour"),
        (span.get_minutes(), "minute"),
        (span.get_seconds(), "second"),
    ];
    let parts: Vec<String> = units
        .iter()
        .skip_while(|(n, _)| *n == 0)
        .take(2)
        .filter(|(n, _)| *n != 0)
        .map(|(n, unit)| {
            if *n == 1 {
                format!("1 {unit}")
            } else {
                format!("{n} {unit}s")
            }
        })
        .collect();
    let text = parts.join(" ");
    if diff < 0 {
        format!("{text} ago")
    } else {
        format!("in {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    fn now() -> Timestamp {
        "2026-11-03T21:00:00Z".parse().unwrap()
    }

    fn at(s: &str) -> Reading {
        read(s, None, &ny(), now()).unwrap()
    }

    fn utc(s: &str) -> String {
        at(s).at.to_string()
    }

    #[test]
    fn the_unit_comes_from_the_size_and_is_said() {
        let r = at("1730642400");
        assert_eq!(
            r.read_as,
            ReadAs::Unix {
                unit: Unit::Seconds,
                guessed: true
            }
        );
        assert_eq!(r.at.to_string(), "2024-11-03T14:00:00Z");
        for (s, unit) in [
            ("1730642400000", Unit::Milliseconds),
            ("1730642400000000", Unit::Microseconds),
            ("1730642400000000000", Unit::Nanoseconds),
        ] {
            let r = at(s);
            assert_eq!(r.at.to_string(), "2024-11-03T14:00:00Z", "{s}");
            assert_eq!(
                r.read_as,
                ReadAs::Unix {
                    unit,
                    guessed: true
                }
            );
        }
    }

    #[test]
    fn a_unit_can_be_given() {
        let r = read("1730642400000", Some(Unit::Seconds), &ny(), now());
        assert!(r.unwrap_err().contains("outside the years"));
        let r = read("1730642400", Some(Unit::Milliseconds), &ny(), now()).unwrap();
        assert_eq!(r.at.to_string(), "1970-01-21T00:44:02.4Z");
        assert!(read("2024-01-01", Some(Unit::Seconds), &ny(), now())
            .unwrap_err()
            .contains("isn't a number"));
    }

    #[test]
    fn fractions_and_negatives_are_exact() {
        assert_eq!(
            utc("1730642400.123456789"),
            "2024-11-03T14:00:00.123456789Z"
        );
        assert_eq!(utc("-1"), "1969-12-31T23:59:59Z");
        assert_eq!(utc("-86400.5"), "1969-12-30T23:59:59.5Z");
        let (s, ms) = unix("1969-12-31T23:59:59.5Z".parse().unwrap());
        assert_eq!((s.as_str(), ms), ("-0.5", -500));
        let (s, _) = unix("1969-12-31T23:59:59.5Z".parse().unwrap());
        assert_eq!(s, "-0.5");
    }

    #[test]
    fn a_year_alone_gets_a_note() {
        let r = at("2024");
        assert!(
            r.notes[0].contains("for the year, write 2024-01-01"),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn iso_and_rfc_forms_with_a_zone() {
        assert_eq!(utc("2024-02-29T12:00:00Z"), "2024-02-29T12:00:00Z");
        assert_eq!(utc("2024-02-29T12:00:00+01:00"), "2024-02-29T11:00:00Z");
        assert_eq!(utc("2024-02-29 12:00:00-05:00"), "2024-02-29T17:00:00Z");
        assert_eq!(
            utc("Tue, 1 Jul 2003 10:52:37 +0200"),
            "2003-07-01T08:52:37Z"
        );
        assert_eq!(utc("2024-07-01T12:00[Asia/Tokyo]"), "2024-07-01T03:00:00Z");
        assert_eq!(utc("2024-07-01 12:00 Asia/Tokyo"), "2024-07-01T03:00:00Z");
        assert_eq!(utc("2024-07-01 12:00 UTC"), "2024-07-01T12:00:00Z");
        assert_eq!(utc("2024-07-01 12:00 +0530"), "2024-07-01T06:30:00Z");
    }

    #[test]
    fn a_date_without_a_zone_is_local_and_says_so() {
        let r = at("2024-02-29 12:00");
        assert_eq!(r.at.to_string(), "2024-02-29T17:00:00Z");
        assert_eq!(
            r.read_as,
            ReadAs::Date {
                assumed_zone: Some("America/New_York".to_string())
            }
        );
        assert_eq!(utc("2024-07-04"), "2024-07-04T04:00:00Z");
    }

    #[test]
    fn common_written_forms() {
        for s in [
            "29 Feb 2024",
            "29 February 2024",
            "Feb 29, 2024",
            "February 29 2024",
            "2024/02/29",
            "29.02.2024",
            "29/02/2024",
            "02/29/2024",
            "Thu Feb 29 2024",
        ] {
            assert_eq!(utc(s), "2024-02-29T05:00:00Z", "{s}");
        }
        assert_eq!(utc("Feb 29, 2024 3:30 PM"), "2024-02-29T20:30:00Z");
        assert_eq!(utc("29 Feb 2024 15:30"), "2024-02-29T20:30:00Z");
        assert_eq!(utc("Thu Feb 29 15:30:00 2024"), "2024-02-29T20:30:00Z");
        assert_eq!(utc("Thu Feb 29 15:30:00 UTC 2024"), "2024-02-29T15:30:00Z");
    }

    #[test]
    fn an_ambiguous_slash_date_is_refused_with_both_readings() {
        let e = read("03/04/2024", None, &ny(), now()).unwrap_err();
        assert!(e.contains("4 March or 3 April"), "{e}");
        assert!(e.contains("2024-03-04 or 2024-04-03"), "{e}");
        // The same either way round.
        assert_eq!(utc("05/05/2024"), "2024-05-05T04:00:00Z");
    }

    #[test]
    fn leap_days_and_impossible_dates() {
        assert_eq!(utc("2000-02-29"), "2000-02-29T05:00:00Z");
        let e = read("2023-02-29", None, &ny(), now()).unwrap_err();
        assert!(e.contains("2023 isn't a leap year"), "{e}");
        let e = read("1900-02-29 10:00", None, &ny(), now()).unwrap_err();
        assert!(e.contains("1900 isn't a leap year"), "{e}");
        let e = read("2024-04-31", None, &ny(), now()).unwrap_err();
        assert!(e.contains("April 2024 has 30 days"), "{e}");
        let e = read("31/04/2024", None, &ny(), now()).unwrap_err();
        assert!(e.contains("April 2024 has 30 days"), "{e}");
    }

    #[test]
    fn a_skipped_hour_is_read_after_the_gap() {
        // US clocks went from 02:00 to 03:00 on 10 March 2024.
        let r = at("2024-03-10 02:30");
        assert_eq!(r.at.to_string(), "2024-03-10T07:30:00Z");
        assert!(
            r.notes[0].contains("never happened in America/New_York"),
            "{:?}",
            r.notes
        );
        assert!(r.notes[0].contains("03:30 EDT"), "{:?}", r.notes);
    }

    #[test]
    fn a_repeated_hour_is_read_as_the_first() {
        // And back from 02:00 to 01:00 on 3 November 2024.
        let r = at("2024-11-03 01:30");
        assert_eq!(r.at.to_string(), "2024-11-03T05:30:00Z");
        assert!(r.notes[0].contains("happened twice"), "{:?}", r.notes);
        assert!(r.notes[0].contains("01:30 EDT"), "{:?}", r.notes);
    }

    #[test]
    fn zone_abbreviations_are_refused() {
        let e = read("2024-07-01 12:00 EST", None, &ny(), now()).unwrap_err();
        assert!(e.contains("could be more than one time zone"), "{e}");
    }

    #[test]
    fn relative_input() {
        assert_eq!(utc("3 days ago"), "2026-10-31T21:00:00Z");
        assert_eq!(utc("in 2 hours"), "2026-11-03T23:00:00Z");
        assert_eq!(utc("2h ago"), "2026-11-03T19:00:00Z");
        assert_eq!(utc("yesterday"), "2026-11-02T05:00:00Z");
        assert_eq!(utc("now"), "2026-11-03T21:00:00Z");
        assert_eq!(at("now").read_as, ReadAs::Now);
    }

    #[test]
    fn nonsense_says_what_conv_reads() {
        let e = read("next blue moon", None, &ny(), now()).unwrap_err();
        assert!(e.contains("can't read"), "{e}");
    }

    #[test]
    fn relative_descriptions() {
        let n = now();
        let rel = |s: &str| relative(s.parse().unwrap(), n);
        assert_eq!(rel("2026-11-03T21:00:00.4Z"), "now");
        assert_eq!(rel("2026-10-31T21:00:00Z"), "3 days ago");
        assert_eq!(rel("2026-11-03T23:05:00Z"), "in 2 hours 5 minutes");
        assert_eq!(rel("2025-09-03T21:00:00Z"), "1 year 2 months ago");
        assert_eq!(rel("2026-11-03T20:59:30Z"), "30 seconds ago");
    }

    #[test]
    fn a_relative_time_counts_real_hours_across_a_clock_change() {
        // 2024-11-02 12:00 EDT to 2024-11-03 12:00 EST is 25 hours.
        let then: Timestamp = "2024-11-02T16:00:00Z".parse().unwrap();
        let now: Timestamp = "2024-11-03T17:00:00Z".parse().unwrap();
        assert_eq!(relative(then, now), "1 day 1 hour ago");
        let r = read("1 day ago", None, &ny(), now).unwrap();
        assert_eq!(r.at.to_string(), "2024-11-02T17:00:00Z");
    }

    #[test]
    fn shown_in_a_zone() {
        let s = show("2026-11-03T21:00:00Z".parse().unwrap(), &ny());
        assert_eq!(s.human, "Tue 2026-11-03 16:00:00 EST");
        assert_eq!(s.offset, "-05:00");
        assert_eq!(s.rfc3339, "2026-11-03T16:00:00-05:00");
        assert_eq!(s.zone, "America/New_York");
        let s = show("2024-02-29T12:00:00.25Z".parse().unwrap(), &TimeZone::UTC);
        assert_eq!(s.human, "Thu 2024-02-29 12:00:00.25 UTC");
    }
}
