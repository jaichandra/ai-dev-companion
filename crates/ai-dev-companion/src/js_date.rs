//! `Date.parse` for the timestamps the feeds publish. Handles ISO 8601
//! (`2026-09-29`, `2026-09-29T02:00:00Z`, with fractional seconds and a
//! `+hh:mm` offset). JS reads a date-time with no offset as LOCAL time; this
//! reads it as UTC (the services publishing these always send an offset).
//! Other formats JS would accept (RFC 2822, "Sep 29 2026") are not parsed:
//! the caller treats them as invalid.

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn num(s: &str, len: usize) -> Option<i64> {
    (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// Milliseconds since the epoch, or `None` when it isn't a date.
pub fn parse_date_ms(text: &str) -> Option<i64> {
    let t = text.trim();
    let (date, time) = match t.split_once(['T', ' ']) {
        Some((d, rest)) => (d, Some(rest)),
        None => (t, None),
    };
    let mut parts = date.split('-');
    let (y, m, d) = (num(parts.next()?, 4)?, num(parts.next()?, 2)?, num(parts.next()?, 2)?);
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, m, d) * 86_400_000;
    if let Some(time) = time {
        let (clock, offset_min) = if let Some(c) = time.strip_suffix(['Z', 'z']) {
            (c, 0)
        } else if let Some(i) = time.rfind(['+', '-']) {
            let (c, off) = time.split_at(i);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let (oh, om) = off[1..].split_once(':')?;
            (c, sign * (num(oh, 2)? * 60 + num(om, 2)?))
        } else {
            (time, 0)
        };
        let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
        let mut hp = hms.split(':');
        let (h, mi) = (num(hp.next()?, 2)?, num(hp.next()?, 2)?);
        let s = hp.next().map_or(Some(0), |s| num(s, 2))?;
        if hp.next().is_some() || h > 24 || mi > 59 || s > 59 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let millis = if frac.is_empty() { 0 } else { format!("{:0<3}", &frac[..frac.len().min(3)]).parse::<i64>().ok()? };
        ms += ((h * 60 + mi) * 60 + s) * 1000 + millis - offset_min * 60_000;
    }
    Some(ms)
}

/// `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ`, with a signed
/// six-digit year outside 0000-9999. `None` outside the range JS dates cover.
pub fn to_iso_string(ms: i64) -> Option<String> {
    if ms.unsigned_abs() > 8_640_000_000_000_000 {
        return None;
    }
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let year = if (0..=9999).contains(&y) { format!("{y:04}") } else { format!("{}{:06}", if y < 0 { "-" } else { "+" }, y.abs()) };
    Some(format!("{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_8601() {
        assert_eq!(parse_date_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_date_ms("1970-01-02"), Some(86_400_000));
        assert_eq!(parse_date_ms("2026-09-29T02:00:00Z"), Some(1_790_647_200_000));
        assert_eq!(parse_date_ms("2026-09-29T02:00:00.5Z"), Some(1_790_647_200_500));
        assert_eq!(parse_date_ms("2026-09-29T04:00:00+02:00"), Some(1_790_647_200_000));
        assert_eq!(parse_date_ms("2026-09-28T22:00:00-04:00"), Some(1_790_647_200_000));
        assert_eq!(parse_date_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
    }

    #[test]
    fn formats_like_to_iso_string_and_round_trips_with_the_parser() {
        for (ms, want) in [
            (0, "1970-01-01T00:00:00.000Z"),
            (1_790_647_200_000, "2026-09-29T02:00:00.000Z"),
            (1_790_647_200_500, "2026-09-29T02:00:00.500Z"),
            (-86_400_000, "1969-12-31T00:00:00.000Z"),
            (1_709_164_800_000, "2024-02-29T00:00:00.000Z"),
            (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
            (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
            (-62_198_755_200_000, "-000001-01-01T00:00:00.000Z"),
        ] {
            assert_eq!(to_iso_string(ms).as_deref(), Some(want), "{ms}");
        }
        assert_eq!(to_iso_string(8_640_000_000_000_001), None);
        for ms in [0, 1_790_647_200_123, -5_000_000_000_000, 4_102_444_800_000] {
            assert_eq!(parse_date_ms(&to_iso_string(ms).unwrap()), Some(ms), "{ms}");
        }
    }

    #[test]
    fn rejects_what_is_not_a_date() {
        for bad in ["not a date", "", "2026-13-01", "2026-09-29T25:00:00Z", "2026-9-29", "2026-09-29T02:00:00+0200", "x-y-z"] {
            assert_eq!(parse_date_ms(bad), None, "{bad:?}");
        }
    }
}
