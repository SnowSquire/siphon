use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(module: &str, message: impl std::fmt::Display) {
    eprintln!("[{ts}][{module}] {message}", ts = timestamp());
}

/// RFC 3339 UTC timestamp, matching the TS client's `toISOString()` output.
pub fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((elapsed.as_secs() / 86_400) as i64);
    let rem = elapsed.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        elapsed.subsec_millis()
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 -> y/m/d).
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
