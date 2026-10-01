//! Minimal UTC date helpers so `history` can print RFC 3339 times and
//! accept `--since YYYY-MM-DD` without pulling in a date crate.

/// Format unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn format_rfc3339(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Parse a `--since` value: either unix seconds (`1790880000`) or a UTC
/// calendar date (`2026-10-01`, meaning midnight UTC).
pub fn parse_since(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let bad = || format!("invalid date {:?}: use unix seconds or YYYY-MM-DD", s);
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<u64>().map_err(|_| bad());
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad());
    }
    let y: i64 = parts[0].parse().map_err(|_| bad())?;
    let m: u32 = parts[1].parse().map_err(|_| bad())?;
    let d: u32 = parts[2].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(bad());
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return Err(bad());
    }
    Ok(days as u64 * 86_400)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's civil-date algorithms (public domain).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_known_timestamps() {
        // Values cross-checked with `date -u -r <ts>`.
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339(1_753_440_135), "2025-07-25T10:42:15Z");
        // Leap day.
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        // Last second of a year.
        assert_eq!(format_rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn parses_unix_seconds() {
        assert_eq!(parse_since("1790880000").unwrap(), 1_790_880_000);
        assert_eq!(parse_since(" 0 ").unwrap(), 0);
    }

    #[test]
    fn parses_dates_as_utc_midnight() {
        assert_eq!(parse_since("1970-01-01").unwrap(), 0);
        assert_eq!(parse_since("2024-02-29").unwrap(), 1_709_164_800);
        assert_eq!(parse_since("2023-11-14").unwrap(), 1_699_920_000);
    }

    #[test]
    fn date_round_trips_through_format() {
        for ts in [0u64, 951_782_400 /* 2000-02-29 */, 1_709_164_800, 4_102_444_800 /* 2100-01-01 */] {
            let day = &format_rfc3339(ts)[..10];
            assert_eq!(parse_since(day).unwrap(), ts, "{day}");
        }
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "yesterday", "2026-13-01", "2026-02-30", "2023-02-29", "2026-1-1x", "1969-12-31", "-5", "2026/10/01"] {
            assert!(parse_since(bad).is_err(), "should reject {bad:?}");
        }
    }
}
