//! Minimal SHA-1, used only to build Twitch's private DVR playlist path.
//!
//! Twitch names the path `sha1("{channel}_{stream_id}_{start}")[..20]`, so this
//! is not security-sensitive; it just has to match Node's `crypto.createHash`.
//! Keeping it local avoids a dependency for one 40-byte digest.

use std::fmt::Write;

const INITIAL: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];

/// Hex-encoded SHA-1 of `input`, lowercase.
pub fn hex(input: &[u8]) -> String {
    let mut state = INITIAL;
    let bit_len = (input.len() as u64).wrapping_mul(8);

    let (blocks, remainder) = input.as_chunks::<64>();
    for block in blocks {
        compress(&mut state, block);
    }

    // One or two blocks of padding: `0x80`, zeros, then the 64-bit bit length.
    let block_len = if remainder.len() < 56 { 64 } else { 128 };
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    tail[block_len - 8..block_len].copy_from_slice(&bit_len.to_be_bytes());
    let (padded, _) = tail[..block_len].as_chunks::<64>();
    for block in padded {
        compress(&mut state, block);
    }

    let mut out = String::with_capacity(40);
    for word in state {
        let _ = write!(out, "{word:08x}");
    }
    out
}

fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut words = [0u32; 80];
    let (rounds, _) = block.as_chunks::<4>();
    for (word, bytes) in words.iter_mut().zip(rounds) {
        *word = u32::from_be_bytes(*bytes);
    }
    for index in 16..80 {
        words[index] =
            (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                .rotate_left(1);
    }

    let [mut h0, mut h1, mut h2, mut h3, mut h4] = *state;
    for (index, &word) in words.iter().enumerate() {
        let (func, constant) = match index {
            0..=19 => ((h1 & h2) | (!h1 & h3), 0x5A82_7999),
            20..=39 => (h1 ^ h2 ^ h3, 0x6ED9_EBA1),
            40..=59 => ((h1 & h2) | (h1 & h3) | (h2 & h3), 0x8F1B_BCDC),
            _ => (h1 ^ h2 ^ h3, 0xCA62_C1D6),
        };
        let temp = h0
            .rotate_left(5)
            .wrapping_add(func)
            .wrapping_add(h4)
            .wrapping_add(constant)
            .wrapping_add(word);
        h4 = h3;
        h3 = h2;
        h2 = h1.rotate_left(30);
        h1 = h0;
        h0 = temp;
    }

    state[0] = state[0].wrapping_add(h0);
    state[1] = state[1].wrapping_add(h1);
    state[2] = state[2].wrapping_add(h2);
    state[3] = state[3].wrapping_add(h3);
    state[4] = state[4].wrapping_add(h4);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_standard_vectors() {
        assert_eq!(hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn hashes_a_full_block_without_padding() {
        // 64 bytes exactly: the padding needs a second block.
        let input = [b'a'; 64];
        assert_eq!(hex(&input), "0098ba824b5c16427bd7a1122a5a442a25ec644d");
    }
}
