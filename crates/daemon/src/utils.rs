use std::num::NonZeroU16;

use rand::RngExt;
use rand::distr::Alphanumeric;

/// 32 random alphanumeric characters, used for Twitch session ids.
pub fn random_id() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub trait RetainUnordered<T> {
    /// Keeps elements for which `f` returns `true`; order of survivors is not preserved.
    fn retain_unordered<F>(&mut self, f: F)
    where
        F: FnMut(&T) -> bool;
}

impl<T> RetainUnordered<T> for Vec<T> {
    fn retain_unordered<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        let mut i = 0;
        let mut n = self.len();
        while i < n {
            if f(&self[i]) {
                i += 1;
            } else {
                n -= 1;
                self.swap(i, n);
            }
        }
        self.truncate(n);
    }
}

/// Decode standard-alphabet base64 (padding optional, whitespace ignored).
/// `None` on any character outside the alphabet.
#[must_use]
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((accumulator >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        out.push(char::from(ALPHABET[usize::from(first >> 2)]));
        out.push(char::from(
            ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)],
        ));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[usize::from(third & 0x3f)])
        } else {
            '='
        });
    }
    out
}

/// Percent-encodes `value` as a URL query component.
///
/// RFC 3986 unreserved bytes pass through; everything else, including the `+`,
/// `/` and `=` of a base64 signature or token, is escaped.
pub fn encode_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

pub fn query_string(params: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (index, (key, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&encode_component(key));
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out
}

/// Splits an unsigned decimal string into its integer part and its first
/// `digits` fractional digits scaled to `10^digits`. Returns `None` for
/// non-numeric input.
#[inline]
fn split_decimal(s: &str, digits: usize) -> Option<(u64, u64)> {
    const POW10: [u64; 5] = [1, 10, 100, 1000, 10_000];
    debug_assert!(digits <= 4);

    let s = s.trim();
    let s = s.strip_prefix('+').unwrap_or(s);
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));

    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }

    let int: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };

    let mut frac = 0;

    for byte in frac_part.bytes().take(digits) {
        if !byte.is_ascii_digit() {
            return None;
        }

        frac = frac * 10 + u64::from(byte - b'0');
    }

    frac *= POW10[digits - frac_part.len().min(digits)];

    Some((int, frac))
}

/// Parses a decimal string into milliseconds, rounding half up on the fourth
/// fractional digit. Returns `None` for invalid or overflowing input.
pub fn float_string_to_millis(s: &str) -> Option<u64> {
    let (secs, frac) = split_decimal(s, 4)?;
    secs.checked_mul(1000)?.checked_add((frac + 5) / 10)
}

/// Parses a decimal string (e.g. `SCORE` or `FRAME-RATE`) directly into
/// hundredths, returning `None` for missing/invalid/zero/overflowing values.
/// Rounds half up based on the third fractional digit.
pub fn float_string_to_centis(s: &str) -> Option<NonZeroU16> {
    let (int, frac) = split_decimal(s, 3)?;
    let scaled = int.checked_mul(100)?.checked_add((frac + 5) / 10)?;

    NonZeroU16::new(u16::try_from(scaled).ok()?)
}

/// Parses an ISO-8601 timestamp such as Twitch's `2026-11-08T06:37:32.191Z`
/// into milliseconds since the Unix epoch.
///
/// Twitch only emits UTC (`Z`), but an explicit numeric offset is accepted too.
/// Returns `None` for malformed or out-of-range input.
pub fn parse_iso8601_millis(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once('T')?;

    let mut fields = date.split('-');
    let year: i64 = fields.next()?.parse().ok()?;
    let month: u32 = fields.next()?.parse().ok()?;
    let day: u32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (time, offset_minutes) = if let Some(time) = rest.strip_suffix('Z') {
        (time, 0)
    } else if let Some(position) = rest.rfind(['+', '-']) {
        let (time, offset) = rest.split_at(position);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':')?;
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        (time, sign * (hours * 60 + minutes))
    } else {
        (rest, 0)
    };

    let mut fields = time.split(':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next()?.parse().ok()?;
    let seconds = fields.next()?;
    if fields.next().is_some() || hour > 23 || minute > 59 {
        return None;
    }

    let (seconds, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second: i64 = seconds.parse().ok()?;
    if second > 60 {
        return None;
    }

    let mut millis = 0i64;
    for (index, byte) in fraction.bytes().take(3).enumerate() {
        if !byte.is_ascii_digit() {
            return None;
        }
        millis += i64::from(byte - b'0') * [100, 10, 1][index];
    }

    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    seconds
        .checked_mul(1_000)?
        .checked_add(millis)?
        .checked_sub(offset_minutes * 60_000)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Iterates over the `KEY=VALUE` attributes of an HLS tag value, e.g.
/// `URI="main.mp4",BYTERANGE="560@0"`. Quoted values may contain commas.
pub fn attributes(value: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = value;

    std::iter::from_fn(move || {
        rest = rest.trim_start_matches([',', ' ']);
        if rest.is_empty() {
            return None;
        }

        let (key, tail) = rest.split_once('=')?;
        let tail = tail.trim_start();

        let (value, next) = match tail.strip_prefix('"') {
            Some(quoted) => {
                let close = quoted.find('"')?;
                (&quoted[..close], &quoted[close + 1..])
            }
            None => tail
                .split_once(',')
                .map_or((tail, ""), |(value, next)| (value, next)),
        };

        rest = next;
        Some((key.trim(), value.trim()))
    })
}

#[must_use]
pub fn attribute<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    attributes(value).find_map(|(key, value)| (key == name).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_and_round_trips() {
        for case in [&b""[..], b"f", b"fo", b"foo", b"foob", b"foo\r\nbar"] {
            assert_eq!(base64_decode(&base64_encode(case)).as_deref(), Some(case));
        }
        assert_eq!(base64_decode("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode("Zm9v\n"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode("Zm9v!"), None);
        // Truncated input decodes what it can rather than failing: a master with
        // a clipped VALUE should still yield the readable prefix.
        assert!(base64_decode("Zm9").is_some());
    }

    #[test]
    fn encodes_base64_with_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encodes_query_components() {
        assert_eq!(encode_component("abc-._~"), "abc-._~");
        assert_eq!(encode_component("a b+c/d=e"), "a%20b%2Bc%2Fd%3De");
        assert_eq!(query_string(&[("a", "1"), ("b", "2")]), "a=1&b=2");
    }

    #[test]
    fn parses_quoted_attributes_with_commas() {
        let attrs: Vec<_> =
            attributes("CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=60.000")
                .collect();

        assert_eq!(
            attrs,
            [
                ("CODECS", "avc1.4d401f,mp4a.40.2"),
                ("RESOLUTION", "1920x1080"),
                ("FRAME-RATE", "60.000"),
            ]
        );
    }

    #[test]
    fn finds_named_attribute() {
        let attrs = "URI=\"main.mp4\",BYTERANGE=\"560@0\"";

        assert_eq!(attribute(attrs, "URI"), Some("main.mp4"));
        assert_eq!(attribute(attrs, "BYTERANGE"), Some("560@0"));
        assert_eq!(attribute(attrs, "MISSING"), None);
    }

    #[test]
    fn parses_iso8601_timestamps() {
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_millis("2026-11-08T06:37:32.191Z"),
            Some(1_794_119_852_191)
        );
        assert_eq!(
            parse_iso8601_millis("2024-02-29T12:34:56Z"),
            Some(1_709_210_096_000)
        );
        assert_eq!(
            parse_iso8601_millis("2000-01-01T01:00:00+01:00"),
            Some(946_684_800_000)
        );
        assert_eq!(parse_iso8601_millis("not a date"), None);
        assert_eq!(parse_iso8601_millis("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn parses_millis() {
        assert_eq!(float_string_to_millis("2.000"), Some(2000));
        assert_eq!(float_string_to_millis("2"), Some(2000));
        assert_eq!(float_string_to_millis("2."), Some(2000));
        assert_eq!(float_string_to_millis("0.235"), Some(235));
        assert_eq!(float_string_to_millis("5.029"), Some(5029));
        assert_eq!(float_string_to_millis(".5"), Some(500));
        assert_eq!(float_string_to_millis(" 2.5 "), Some(2500));
        // Rounds half up on the fourth fractional digit.
        assert_eq!(float_string_to_millis("2.0005"), Some(2001));
        assert_eq!(float_string_to_millis("2.0004"), Some(2000));
        assert_eq!(float_string_to_millis("0.2355"), Some(236));
    }

    #[test]
    fn rejects_invalid_millis() {
        assert_eq!(float_string_to_millis(""), None);
        assert_eq!(float_string_to_millis("."), None);
        assert_eq!(float_string_to_millis("live"), None);
        assert_eq!(float_string_to_millis("2.5x"), None);
        assert_eq!(float_string_to_millis("2.000x"), None);
        // `${secs} * 1000` must not overflow a `u64`.
        assert_eq!(float_string_to_millis("18446744073709552"), None);
    }

    #[test]
    fn parses_centis() {
        assert_eq!(
            float_string_to_centis("60.000").map(NonZeroU16::get),
            Some(6000)
        );
        assert_eq!(
            float_string_to_centis("59.940").map(NonZeroU16::get),
            Some(5994)
        );
        assert_eq!(
            float_string_to_centis("9.5").map(NonZeroU16::get),
            Some(950)
        );
        assert_eq!(
            float_string_to_centis("+1.2").map(NonZeroU16::get),
            Some(120)
        );
        assert_eq!(
            float_string_to_centis("655.35").map(NonZeroU16::get),
            Some(65535)
        );
    }

    #[test]
    fn rounds_centis_half_up() {
        assert_eq!(
            float_string_to_centis("1.994").map(NonZeroU16::get),
            Some(199)
        );
        assert_eq!(
            float_string_to_centis("1.995").map(NonZeroU16::get),
            Some(200)
        );
        assert_eq!(
            float_string_to_centis("0.005").map(NonZeroU16::get),
            Some(1)
        );
        // Digits past the third never affect the result.
        assert_eq!(
            float_string_to_centis("1.2349").map(NonZeroU16::get),
            Some(123)
        );
    }

    #[test]
    fn rejects_invalid_centis() {
        assert_eq!(float_string_to_centis(""), None);
        assert_eq!(float_string_to_centis("0"), None);
        assert_eq!(float_string_to_centis("0.004"), None);
        assert_eq!(float_string_to_centis("-1"), None);
        assert_eq!(float_string_to_centis("1.2.3"), None);
        assert_eq!(float_string_to_centis("655.36"), None);
    }
}
