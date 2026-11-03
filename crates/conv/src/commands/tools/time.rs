//! `conv time`: Unix timestamps to dates and back.

use convkit_core::tools::time::{self as t, ReadAs, Unit};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde_json::json;

use super::{Common, Done, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Shows a time as a Unix timestamp, in UTC, in your time zone and in any \
other (--tz), and how long ago or from now it is. Without VALUE, now.

A number is a Unix timestamp. Its unit comes from its size (seconds up \
to 11 digits, then milliseconds, microseconds, nanoseconds), and conv \
says which it took; --unit picks another. Before 1970 is negative.

A date can be ISO 8601 (2024-02-29T14:30:00Z, 2024-02-29 14:30), RFC 2822 \
(Thu, 29 Feb 2024 14:30:00 +0000), or written out (29 Feb 2024, Feb 29, \
2024 2:30 PM, 29.02.2024), with an offset or a zone name at the end \
(Asia/Tokyo, UTC). Without one it is read in your time zone, and conv \
says what daylight saving did to it. 03/04/2024 could be either month, \
so conv asks for 2024-03-04 instead. \"3 days ago\", \"in 2 hours\", \
\"yesterday\" work too.

--copy copies the other form: the date for a timestamp, the timestamp \
for a date.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// A Unix timestamp or a date; - reads it from what is piped in.
    /// Without it, now.
    pub value: Option<String>,
    /// Also show the time in this zone, an IANA name like Asia/Tokyo or
    /// America/New_York. Give it again for more.
    #[arg(long, value_name = "ZONE")]
    pub tz: Vec<String>,
    /// Read a number in this unit instead of from its size: s, ms, us or
    /// ns.
    #[arg(long, value_name = "UNIT", value_parser = parse_unit)]
    pub unit: Option<Unit>,
    /// Read the time on the clipboard.
    #[arg(long, conflicts_with = "value")]
    pub paste: bool,
    /// Also copy the other form to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

fn parse_unit(s: &str) -> Result<Unit, String> {
    Unit::parse(s).ok_or_else(|| format!("{s:?} isn't a unit; use s, ms, us or ns"))
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let common = Common {
        file: None,
        paste: args.paste,
        copy: args.copy,
    };
    let given = match super::read(
        cli,
        args.value.as_deref(),
        &common,
        Piped::OnlyWithDash,
        "time",
    )
    .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(g) => g,
        Err(e) => return super::fail(cli, &e),
    };
    let zones: Result<Vec<TimeZone>, _> = args
        .tz
        .iter()
        .map(|z| {
            TimeZone::get(z).map_err(|_| {
                super::usage(format!(
                    "conv doesn't know the time zone {z:?}; give an IANA name like Asia/Tokyo, Europe/Paris or America/New_York"
                ))
            })
        })
        .collect();
    let zones = match zones {
        Ok(z) => z,
        Err(e) => return super::fail(cli, &e),
    };
    let local = TimeZone::system();
    // To the second: now's microseconds are noise here.
    let now = Timestamp::from_second(Timestamp::now().as_second()).expect("now is a time");
    let input = given.as_deref().map(str::trim).unwrap_or("");
    let reading = match t::read(input, args.unit, &local, now) {
        Ok(r) => r,
        Err(e) => return super::fail(cli, &super::failed(e)),
    };
    if let ReadAs::Date {
        assumed_zone: Some(zone),
    } = &reading.read_as
    {
        super::note(cli, &format!("no zone given, so read in yours, {zone}"));
    }
    for n in &reading.notes {
        super::note(cli, n);
    }
    let at = reading.at;
    let (seconds, ms) = t::unix(at);
    let utc = t::show(at, &TimeZone::UTC);
    let here = t::show(at, &local);
    let relative = t::relative(at, now);
    let mut rows = Vec::new();
    if let ReadAs::Unix { unit, guessed } = reading.read_as {
        let digits = input
            .trim_start_matches(['-', '+'])
            .split('.')
            .next()
            .unwrap_or("")
            .len();
        let how = if guessed {
            let digits = convkit_core::tools::plural(digits as u64, "digit", "digits");
            format!("{} ({digits}; --unit picks another)", unit.name())
        } else {
            unit.name().to_string()
        };
        rows.push(("read as".to_string(), how));
    }
    rows.push(("unix".to_string(), seconds.clone()));
    rows.push(("unix ms".to_string(), ms.to_string()));
    rows.push(("utc".to_string(), utc.human.clone()));
    rows.push((
        "local".to_string(),
        format!("{} ({}, {})", here.human, here.offset, here.zone),
    ));
    let mut zone_json = Vec::new();
    for tz in &zones {
        let s = t::show(at, tz);
        rows.push((s.zone.clone(), format!("{} ({})", s.human, s.offset)));
        zone_json.push(json!({ "zone": s.zone, "time": s.rfc3339 }));
    }
    rows.push(("iso 8601".to_string(), at.to_string()));
    rows.push(("relative".to_string(), relative.clone()));
    // The other form: the date for a timestamp, the timestamp for a date.
    let copy = match reading.read_as {
        ReadAs::Unix { .. } => at.to_string(),
        _ => seconds.clone(),
    };
    let read_as = match &reading.read_as {
        ReadAs::Now => json!("now"),
        ReadAs::Unix { unit, .. } => json!(unit.name()),
        ReadAs::Date { .. } => json!("date"),
        ReadAs::Relative => json!("relative"),
    };
    let json = json!({
        "times": [{
            "input": given,
            "read_as": read_as,
            "unit_guessed": matches!(reading.read_as, ReadAs::Unix { guessed: true, .. }),
            "unix": at.as_second(),
            "unix_ms": ms,
            "utc": at.to_string(),
            "local": { "zone": here.zone, "time": here.rfc3339 },
            "zones": zone_json,
            "relative": relative,
            "notes": reading.notes,
        }]
    });
    super::finish(
        cli,
        args.copy,
        Done::new(super::rows(&rows), Some(copy), json),
    )
}
