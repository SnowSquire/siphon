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

#[allow(clippy::cast_possible_truncation)]
pub fn float_string_to_millis(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, "0"));
    let frac = if frac.is_empty() { "0" } else { frac };

    let digits = frac.len().min(3) as u32;
    let frac_ms: u64 = frac.get(..digits as usize)?.parse().ok()?;

    Some(secs.parse::<u64>().ok()? * 1000 + frac_ms * 10u64.pow(3 - digits))
}

/// Parses a decimal string (e.g. `SCORE` or `FRAME-RATE`) directly into
/// hundredths, returning `None` for missing/invalid/zero/overflowing values.
/// Rounds half up based on the third fractional digit.
pub fn float_string_to_centis(s: &str) -> Option<NonZeroU16> {
    let trimmed = s.trim();
    let s = trimmed.strip_prefix('+').unwrap_or(trimmed);
    if s.starts_with('-') {
        return None;
    }

    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let int: u32 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };

    let mut frac_bytes = frac_part.bytes();
    let d1 = frac_bytes.next().map_or(0, |b| u32::from(b - b'0'));
    let d2 = frac_bytes.next().map_or(0, |b| u32::from(b - b'0'));
    let round_up = frac_bytes.next().is_some_and(|b| b >= b'5');

    let scaled = int
        .checked_mul(100)?
        .checked_add(d1 * 10 + d2 + u32::from(round_up))?;

    NonZeroU16::new(u16::try_from(scaled).ok()?)
}
