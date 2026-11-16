//! Small UTC and size formatters for the UI, without a formatting crate.

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
