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
    if let Some(rest) = url.strip_prefix("https://") {
        const HTTP_PREFIX: &[u8; 7] = b"http://";
        let mut buffer = Box::<[u8]>::new_uninit_slice(url.len() - 1);

        let dest = buffer.as_mut_ptr().cast::<u8>();
        unsafe {
            ptr::copy_nonoverlapping(HTTP_PREFIX.as_ptr(), dest, HTTP_PREFIX.len());
            ptr::copy_nonoverlapping(rest.as_ptr(), dest.add(HTTP_PREFIX.len()), rest.len());
            std::mem::transmute::<Box<[u8]>, Box<str>>(buffer.assume_init())
        }
    } else {
        Box::from(url)
    }
}

pub fn float_string_to_millis(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, "0"));
    let frac = if frac.is_empty() { "0" } else { frac };

    let digits = frac.len().min(3) as u32;
    let frac_ms: u64 = frac.get(..digits as usize)?.parse().ok()?;

    Some(secs.parse::<u64>().ok()? * 1000 + frac_ms * 10u64.pow(3 - digits))
}
