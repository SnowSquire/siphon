//! Calendar math for GQL timestamps and log/update times, without a date crate.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Milliseconds since the unix epoch.
pub fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// RFC 3339 UTC timestamp with millisecond precision.
pub fn now_iso() -> String {
    format_iso_ms(now_ms())
}

/// Millis-since-epoch to `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_iso_ms(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60,
        millis.rem_euclid(1000),
    )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the civil-date algorithm keeps month, day and year in range"
)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "month and day are parsed from fixed-width, in-range date fields"
)]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * i64::from(mp) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the unix epoch for an ISO 8601 UTC timestamp like
/// `2026-09-10T18:35:06Z`; fractional seconds are accepted and truncated to
/// millisecond precision.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "month and day are two-digit fields, so they cannot truncate"
)]
#[must_use]
pub fn parse_iso_ms(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| raw.get(range)?.parse::<i64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    let millis = if bytes.len() > 19 && bytes[19] == b'.' {
        let end = bytes[20..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map_or(bytes.len(), |offset| 20 + offset);
        let digits = raw.get(20..end)?;
        let leading = &digits[..digits.len().min(3)];
        let mut millis = leading.parse::<i64>().ok()?;
        for _ in leading.len()..3 {
            millis *= 10;
        }
        millis
    } else {
        0
    };
    Some(
        days_from_civil(year, month as u32, day as u32) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1000
            + millis,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_without_a_date_crate() {
        assert_eq!(format_iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso_ms(1_000_000_000_000), "2001-09-09T01:46:40.000Z");
        assert_eq!(
            format_iso_ms(1_709_208_000_000),
            "2024-02-29T12:00:00.000Z"
        );
        assert_eq!(
            format_iso_ms(1_788_237_802_868),
            "2026-09-01T04:43:22.868Z"
        );
    }

    #[test]
    fn parses_iso_and_round_trips() {
        for raw in [
            "2026-09-10T18:35:06Z",
            "2026-09-10T18:35:06.5Z",
            "2026-09-10T18:35:06.123456Z",
        ] {
            let millis = parse_iso_ms(raw).expect(raw);
            let formatted = format_iso_ms(millis);
            assert_eq!(&formatted[..19], &raw[..19], "{raw}");
        }
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(parse_iso_ms("2026/09/10T18:35:06Z"), None);
    }
}
