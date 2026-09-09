//! Calendar arithmetic, enough for JSF timestamps and ISO-8601 output.
//!
//! Deliberately dependency-free: the only calendar questions this codebase asks
//! are "what month is day-of-year N" and "print this instant as ISO-8601".

pub fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

const MDAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Day-of-year (1-based) -> (month, day), or None if out of range.
pub fn month_day(year: i32, doy: u32) -> Option<(u32, u32)> {
    let leap = is_leap(year);
    let mut rem = doy;
    if rem == 0 || rem > if leap { 366 } else { 365 } {
        return None;
    }
    for (i, &d) in MDAYS.iter().enumerate() {
        let d = if i == 1 && leap { 29 } else { d };
        if rem <= d {
            return Some((i as u32 + 1, rem));
        }
        rem -= d;
    }
    None
}

/// Days from 1970-01-01 to the given civil date. Howard Hinnant's algorithm.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// Unix seconds for a UTC calendar instant.
pub fn unix_from_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, mo, d) * 86400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

/// ISO-8601 UTC, milliseconds included when the fractional part is non-zero.
pub fn iso8601(unix: f64) -> String {
    let whole = unix.floor();
    let frac = unix - whole;
    let secs = whole as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let ms = (frac * 1000.0).round() as i64;
    if ms > 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }
}

/// `HH:MM:SS` for a duration in seconds.
pub fn hms(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as i64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}
