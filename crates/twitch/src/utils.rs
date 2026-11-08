use std::num::NonZeroU16;

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

/// Builds `key=value&...` with both sides percent-encoded.
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

/// Finds one attribute by name in an HLS tag value.
#[must_use]
pub fn attribute<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    attributes(value).find_map(|(key, value)| (key == name).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

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
