//! Calendar time for container metadata: FFmpeg's `creation_time` text
//! (`2024-05-01T12:30:00.000000Z`) to and from microseconds since the Unix
//! epoch, which MP4 (`mvhd`, seconds since 1904) and Matroska (`DateUTC`,
//! nanoseconds since 2001) each offset from.

/// Seconds from 1904-01-01 (the MP4 / QuickTime epoch) to the Unix epoch.
pub const MP4_EPOCH_OFFSET: i64 = 2_082_844_800;
/// Seconds from the Unix epoch to 2001-01-01 (the Matroska `DateUTC` epoch).
pub const MATROSKA_EPOCH_OFFSET: i64 = 978_307_200;

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Parse an ISO 8601 / RFC 3339 UTC time (`YYYY-MM-DD`, optionally
/// `THH:MM:SS` or ` HH:MM:SS`, an optional fraction, an optional `Z` or
/// `±HH:MM` offset) into microseconds since the Unix epoch.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        (!t.is_empty() && t.bytes().all(|c| c.is_ascii_digit()))
            .then(|| t.parse().ok())
            .flatten()
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400;
    let mut micros = 0i64;
    let mut rest = &s[10..];
    if let Some(t) = rest.strip_prefix(['T', 't', ' ']) {
        let tb = t.as_bytes();
        if tb.len() < 8 || tb[2] != b':' || tb[5] != b':' {
            return None;
        }
        let part = |r: std::ops::Range<usize>| -> Option<i64> {
            let v = t.get(r)?;
            v.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| v.parse().ok())?
        };
        let (h, mi, se) = (part(0..2)?, part(3..5)?, part(6..8)?);
        if h > 23 || mi > 59 || se > 60 {
            return None;
        }
        secs += h * 3600 + mi * 60 + se;
        rest = &t[8..];
        if let Some(f) = rest.strip_prefix(['.', ',']) {
            let digits = f.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            let mut v = 0i64;
            for (i, c) in f.bytes().take(digits).enumerate() {
                if i < 6 {
                    v = v * 10 + i64::from(c - b'0');
                }
            }
            for _ in digits..6 {
                v *= 10;
            }
            micros = v;
            rest = &f[digits..];
        }
    }
    match rest {
        "" | "Z" | "z" => {}
        o if (o.starts_with('+') || o.starts_with('-')) && o.len() == 6 && &o[3..4] == ":" => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            secs -= sign * (oh * 3600 + om * 60);
        }
        _ => return None,
    }
    secs.checked_mul(1_000_000)?.checked_add(micros)
}

/// Microseconds since the Unix epoch as FFmpeg writes `creation_time`:
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
pub fn format_iso8601(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_round_trips_and_knows_the_epochs() {
        let t = parse_iso8601("2024-02-29T23:59:58.123456Z").unwrap();
        assert_eq!(format_iso8601(t), "2024-02-29T23:59:58.123456Z");
        assert_eq!(parse_iso8601("1970-01-01"), Some(0));
        assert_eq!(
            parse_iso8601("2001-01-01T00:00:00Z"),
            Some(MATROSKA_EPOCH_OFFSET * 1_000_000)
        );
        assert_eq!(
            parse_iso8601("1904-01-01 00:00:00"),
            Some(-MP4_EPOCH_OFFSET * 1_000_000)
        );
        assert_eq!(
            parse_iso8601("2020-06-01T12:00:00+02:00"),
            parse_iso8601("2020-06-01T10:00:00Z")
        );
        assert_eq!(parse_iso8601("not a date"), None);
        assert_eq!(parse_iso8601("2020-13-01"), None);
    }
}
