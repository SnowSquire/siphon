//! HDR math for the presentation.
//!
//! The interface is SDR, but on an HDR output the chain presents PQ, so a
//! diagnostics part has to be able to say "this many nits" in the encoding the
//! backbuffer uses rather than in interface values.

/// The PQ (SMPTE ST 2084) signal for `nits`, normalized to 0..1.
///
/// Values above the 10 000-nit ceiling saturate, and non-positive input is
/// black: there is no meaningful negative luminance.
#[must_use]
pub fn pq_signal(nits: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = (2523.0 / 4096.0) * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = (2413.0 / 4096.0) * 32.0;
    const C3: f32 = (2392.0 / 4096.0) * 32.0;

    let normalized = (nits / 10_000.0).clamp(0.0, 1.0);
    let power = normalized.powf(M1);
    (C2.mul_add(power, C1) / C3.mul_add(power, 1.0)).powf(M2)
}

#[cfg(test)]
mod tests {
    use super::pq_signal;

    #[test]
    fn matches_the_published_values() {
        // 100 nits is the reference point; 1000 and 10 000 are the usual
        // highlights, and the curve has to reach exactly 1.0 at the ceiling.
        assert!((pq_signal(100.0) - 0.508).abs() < 0.002);
        assert!((pq_signal(1000.0) - 0.751).abs() < 0.002);
        assert!((pq_signal(10_000.0) - 1.0).abs() < 1e-4);
        assert!(pq_signal(0.0) < 0.01);
    }
}
