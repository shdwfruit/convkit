//! A zip's plain timestamp is the MS-DOS one: local wall-clock time, to
//! two seconds, from 1980 to 2107, with no time zone. conv also writes and
//! prefers the extended timestamp (`UT`, 0x5455), which is UTC seconds and
//! what Info-ZIP and macOS write too; the DOS one is the fallback for zips
//! made without it, and is read as this machine's local time, as every
//! unzip tool does.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since 1970, clamped to what fits.
pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

pub(crate) fn from_unix_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// (year, month, day, hour, minute, second) of `secs` in local time.
#[cfg(unix)]
fn local_parts(secs: i64) -> Option<(i32, u32, u32, u32, u32, u32)> {
    // `time_t` is 32 bits on some systems.
    #[allow(clippy::useless_conversion)]
    let t: libc::time_t = secs.try_into().ok()?;
    // SAFETY: `tm` is plain data that localtime_r fills in.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    ))
}

/// Seconds since 1970 of a local wall-clock time.
#[cfg(unix)]
fn from_local_parts(p: (i32, u32, u32, u32, u32, u32)) -> Option<i64> {
    // SAFETY: plain data, filled in below.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = p.0 - 1900;
    tm.tm_mon = p.1 as i32 - 1;
    tm.tm_mday = p.2 as i32;
    tm.tm_hour = p.3 as i32;
    tm.tm_min = p.4 as i32;
    tm.tm_sec = p.5 as i32;
    // Let the C library decide whether summer time was in force.
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a valid, initialised struct.
    let t = unsafe { libc::mktime(&mut tm) };
    (t != -1).then_some(t as i64)
}

// Windows: read and write the DOS time as UTC. Every zip conv writes
// carries the exact UTC time as well, so only a zip made elsewhere
// without one can come out a time-zone offset off.
#[cfg(not(unix))]
fn local_parts(secs: i64) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    Some((y, m, d, rem / 3600, rem / 60 % 60, rem % 60))
}

#[cfg(not(unix))]
fn from_local_parts(p: (i32, u32, u32, u32, u32, u32)) -> Option<i64> {
    let days = days_from_civil(p.0, p.1, p.2);
    Some(days * 86_400 + i64::from(p.3 * 3600 + p.4 * 60 + p.5))
}

/// Howard Hinnant's `civil_from_days`: days since 1970 to a date.
#[cfg(not(unix))]
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

#[cfg(not(unix))]
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The DOS timestamp for `t`, clamped into the range it can hold.
pub(crate) fn to_dos(t: SystemTime) -> zip::DateTime {
    let parts = local_parts(unix_secs(t));
    let Some((y, mo, d, h, mi, s)) = parts else {
        return zip::DateTime::default();
    };
    if y < 1980 {
        return zip::DateTime::default();
    }
    if y > 2107 {
        return zip::DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58).unwrap_or_default();
    }
    zip::DateTime::from_date_and_time(y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8)
        .unwrap_or_default()
}

/// The time a DOS timestamp names, read as local time.
pub(crate) fn from_dos(dt: zip::DateTime) -> Option<SystemTime> {
    let p = (
        i32::from(dt.year()),
        u32::from(dt.month()),
        u32::from(dt.day()),
        u32::from(dt.hour()),
        u32::from(dt.minute()),
        u32::from(dt.second()),
    );
    from_local_parts(p).map(from_unix_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dos_time_round_trips_to_the_even_second() {
        // 2024-03-05 14:07:22 UTC, an even second, which DOS can hold.
        let t = from_unix_secs(1_709_647_642);
        let back = from_dos(to_dos(t)).unwrap();
        assert_eq!(unix_secs(back), 1_709_647_642);
    }

    #[test]
    fn a_time_before_1980_is_clamped_to_1980() {
        let dt = to_dos(from_unix_secs(0));
        assert_eq!(dt.year(), 1980);
    }

    #[cfg(not(unix))]
    #[test]
    fn civil_dates_round_trip() {
        for days in [-1000, 0, 1, 19_000, 50_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
