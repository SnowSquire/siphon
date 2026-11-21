//! Small UTC and size formatters for the UI, without a formatting crate.

/// Milliseconds since the unix epoch, for the relative labels.
#[must_use]
pub fn now_ms() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Millis-since-epoch to `HH:MM:SS` UTC, for log lines.
#[must_use]
pub fn format_time_ms(millis: i64) -> String {
    let secs = millis.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Milliseconds as `h:mm:ss`, for a recording's position.
#[must_use]
pub fn format_duration_ms(millis: u64) -> String {
    let secs = millis / 1000;
    let hours = secs / 3600;
    if hours == 0 {
        format!("{:02}:{:02}", secs % 3600 / 60, secs % 60)
    } else {
        format!("{hours}:{:02}:{:02}", secs % 3600 / 60, secs % 60)
    }
}

/// A byte count with one decimal place once it leaves the kilobytes.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "a display-only approximation; exact bytes are never needed"
)]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        return format!("{bytes:.0} B");
    }
    if bytes < KIB * KIB {
        return format!("{:.1} KiB", bytes / KIB);
    }
    if bytes < KIB * KIB * KIB {
        return format!("{:.1} MiB", bytes / (KIB * KIB));
    }
    format!("{:.2} GiB", bytes / (KIB * KIB * KIB))
}

/// A viewer count as `12.3k`, so a growing number does not move the column.
#[must_use]
pub fn format_count(count: u32) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        return format!("{:.1}k", f64::from(count) / 1_000.0);
    }
    format!("{:.1}M", f64::from(count) / 1_000_000.0)
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

/// Millis-since-epoch to a bare `YYYY-MM-DD` date.
#[must_use]
pub fn format_date_ms(millis: i64) -> String {
    let (year, month, day) = civil_from_days(millis.div_euclid(1000).div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// How long a live stream has been running, coarsely.
///
/// The clock behind the column ticks every 30 seconds, so seconds would be a
/// lie; minutes are the finest thing it can honestly show. Coarse on purpose,
/// like [`format_ago`], and the same at both ends of a stream.
#[must_use]
pub fn format_uptime(millis: i64) -> String {
    const MINUTE: i64 = 60_000;
    let minutes = millis.max(0) / MINUTE;
    if minutes < 1 {
        return "<1m".to_owned();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h {}m", minutes % 60);
    }
    format!("{}d {}h", hours / 24, hours % 24)
}

/// How long ago an instant was, coarsely.
///
/// The buckets are deliberately wider than the timestamps behind them: the
/// end of a broadcast arrives twice, once from `stream-down` (the moment the
/// event landed) and once from Twitch's `lastBroadcast.startedAt` (the
/// server's idea of the end). They disagree by seconds, and minute-granular
/// text never shows it. A future instant, which is clock skew rather than
/// information, reads as `now`.
#[must_use]
pub fn format_ago(at_ms: i64, now_ms: i64) -> String {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let delta = now_ms.saturating_sub(at_ms);
    if delta < 2 * MINUTE {
        return "now".to_owned();
    }
    if delta < HOUR {
        return format!("{}m ago", delta / MINUTE);
    }
    if delta < DAY {
        return format!("{}h ago", delta / HOUR);
    }
    if delta < 7 * DAY {
        return format!("{}d ago", delta / DAY);
    }
    format_date_ms(at_ms)
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

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    /// The two sources of a broadcast's end disagree by seconds, so the
    /// buckets must not: anything under two minutes is simply "now".
    #[test]
    fn the_fresh_end_of_a_broadcast_is_now_not_a_minute_ago() {
        let now = 1_700_000_000_000;
        assert_eq!(format_ago(now, now), "now");
        assert_eq!(format_ago(now - 119_000, now), "now");
        assert_eq!(format_ago(now - 2 * MINUTE, now), "2m ago");
        assert_eq!(format_ago(now - 59 * MINUTE, now), "59m ago");
        assert_eq!(format_ago(now - HOUR, now), "1h ago");
        assert_eq!(format_ago(now - 23 * HOUR, now), "23h ago");
        assert_eq!(format_ago(now - DAY, now), "1d ago");
        assert_eq!(format_ago(now - 6 * DAY, now), "6d ago");
    }

    #[test]
    fn older_than_a_week_is_a_date_and_a_future_instant_is_now() {
        let now = 1_700_000_000_000;
        assert_eq!(format_ago(now + MINUTE, now), "now");
        assert_eq!(
            format_ago(now - 7 * DAY, now),
            format_date_ms(now - 7 * DAY)
        );
        assert_eq!(format_date_ms(0), "1970-01-01");
        assert_eq!(format_date_ms(951_782_400_000), "2000-02-29");
    }

    /// Uptime is read on the same minute clock as the offline labels, so it
    /// never claims a precision the tick cannot keep.
    #[test]
    fn uptime_rounds_down_to_the_minute() {
        assert_eq!(format_uptime(0), "<1m");
        assert_eq!(format_uptime(59_999), "<1m");
        assert_eq!(format_uptime(MINUTE), "1m");
        assert_eq!(format_uptime(59 * MINUTE), "59m");
        assert_eq!(format_uptime(HOUR), "1h 0m");
        assert_eq!(format_uptime(HOUR + 14 * MINUTE), "1h 14m");
        assert_eq!(format_uptime(23 * HOUR + 59 * MINUTE), "23h 59m");
        assert_eq!(format_uptime(DAY + 2 * HOUR), "1d 2h");
    }
}
