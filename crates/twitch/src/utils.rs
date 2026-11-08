use std::num::NonZeroU16;
use std::ptr;

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

pub fn httpify_url(url: &str) -> Box<str> {
    url.strip_prefix("https://").map_or_else(
        || Box::from(url),
        |rest| {
            const HTTP_PREFIX: &[u8; 7] = b"http://";
            // `url` starts with the 8-byte `"https://"`, so with `rest = &url[8..]`:
            // `url.len() == 8 + rest.len()` and the buffer holds exactly
            // `7 + rest.len()` bytes (`"http://" ++ rest`).
            let mut buffer = Box::<[u8]>::new_uninit_slice(url.len() - 1);

            let dest = buffer.as_mut_ptr().cast::<u8>();
            // SAFETY: `dest` points to a fresh `Box<[MaybeUninit<u8>]>` allocation of
            // `7 + rest.len()` bytes, so bytes `0..7` are valid for writing.
            // `HTTP_PREFIX` is a valid 7-byte static, and source/dest live in
            // distinct allocations, so they cannot overlap.
            unsafe {
                ptr::copy_nonoverlapping(HTTP_PREFIX.as_ptr(), dest, HTTP_PREFIX.len());
            }
            // SAFETY: `dest` starts a `7 + rest.len()`-byte allocation, so offsetting
            // by 7 stays in bounds (at most one-past-the-end when `rest` is empty)
            // within the same allocation.
            let rest_dest = unsafe { dest.add(HTTP_PREFIX.len()) };
            // SAFETY: `rest_dest` has `rest.len()` writable bytes (see buffer sizing
            // above). `rest.as_ptr()` is valid for `rest.len()` reads, and source
            // (`url`'s call-site allocation) and dest (fresh `Box` allocation)
            // cannot overlap.
            unsafe {
                ptr::copy_nonoverlapping(rest.as_ptr(), rest_dest, rest.len());
            }
            // SAFETY: every byte was just initialized above (`7 + rest.len()` bytes
            // covering the whole buffer).
            let buffer = unsafe { buffer.assume_init() };
            // SAFETY: the contents are valid UTF-8: `"http://"` is ASCII and `rest`
            // is the `&str` remainder of `url` after stripping the ASCII `"https://"`
            // prefix, so the split/join is on a char boundary. `Box<[u8]>` and
            // `Box<str>` share layout, so the transmute only adds the UTF-8 invariant.
            unsafe { std::mem::transmute::<Box<[u8]>, Box<str>>(buffer) }
        },
    )
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
