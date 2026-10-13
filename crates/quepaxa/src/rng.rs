//! Seeded ChaCha20 keystream used for proposal priorities.
//!
//! Liveness needs priorities the network cannot predict, so the seed must come from a
//! CSPRNG and be fresh on every start; the same seed always yields the same stream, which
//! makes simulation reproducible. Pure `u32` arithmetic keeps 32- and 64-bit targets equal.

use crate::proposal::LEADER_PRIORITY;

pub(crate) struct ChaCha20 {
    key: [u32; 8],
    counter: u64,
    block: [u32; 16],
    used: usize,
}

impl ChaCha20 {
    pub fn new(seed: [u8; 32]) -> Self {
        let mut key = [0u32; 8];
        let (words, _) = seed.as_chunks::<4>();
        for (k, c) in key.iter_mut().zip(words) {
            *k = u32::from_le_bytes(*c);
        }
        Self {
            key,
            counter: 0,
            block: [0; 16],
            used: 16,
        }
    }

    pub fn next_u32(&mut self) -> u32 {
        if self.used == 16 {
            let c = self.counter;
            self.block = block(&self.key, [c as u32, (c >> 32) as u32, 0, 0]);
            self.counter = c.wrapping_add(1);
            self.used = 0;
        }
        self.used += 1;
        self.block[self.used - 1]
    }

    pub fn next_u64(&mut self) -> u64 {
        u64::from(self.next_u32()) | (u64::from(self.next_u32()) << 32)
    }

    /// Uniform in `[1, H - 1]`: never nil (0) and never the leader's reserved `H`.
    pub fn priority(&mut self) -> u64 {
        loop {
            let p = self.next_u64();
            if p != 0 && p != LEADER_PRIORITY {
                return p;
            }
        }
    }
}

/// One ChaCha20 block (RFC 8439 §2.3); `tail` fills state words 12..16.
fn block(key: &[u32; 8], tail: [u32; 4]) -> [u32; 16] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    init[4..12].copy_from_slice(key);
    init[12..].copy_from_slice(&tail);
    let mut x = init;
    for _ in 0..10 {
        for (a, b, c, d) in [
            (0, 4, 8, 12),
            (1, 5, 9, 13),
            (2, 6, 10, 14),
            (3, 7, 11, 15),
            (0, 5, 10, 15),
            (1, 6, 11, 12),
            (2, 7, 8, 13),
            (3, 4, 9, 14),
        ] {
            x[a] = x[a].wrapping_add(x[b]);
            x[d] = (x[d] ^ x[a]).rotate_left(16);
            x[c] = x[c].wrapping_add(x[d]);
            x[b] = (x[b] ^ x[c]).rotate_left(12);
            x[a] = x[a].wrapping_add(x[b]);
            x[d] = (x[d] ^ x[a]).rotate_left(8);
            x[c] = x[c].wrapping_add(x[d]);
            x[b] = (x[b] ^ x[c]).rotate_left(7);
        }
    }
    for (o, i) in x.iter_mut().zip(init) {
        *o = o.wrapping_add(i);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn seq_seed() -> [u8; 32] {
        core::array::from_fn(|i| i as u8)
    }

    fn le_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    // Expected bytes produced independently with `openssl enc -chacha20`.
    #[test]
    fn block_matches_rfc8439_test_vector() {
        let rng = ChaCha20::new(seq_seed());
        let out = block(&rng.key, [1, 0x0900_0000, 0x4a00_0000, 0]);
        assert_eq!(
            le_bytes(&out[..8]),
            hex("10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e")
        );
    }

    #[test]
    fn stream_starts_at_counter_zero() {
        let mut rng = ChaCha20::new(seq_seed());
        let words: Vec<u32> = (0..8).map(|_| rng.next_u32()).collect();
        assert_eq!(
            le_bytes(&words),
            hex("39fd2b7dd9c5196a8dbd0377b8dc4a498a35d86fbcde6accb2cc7d4cd8ea2492")
        );
    }

    #[test]
    fn same_seed_gives_same_priorities() {
        let (mut a, mut b) = (ChaCha20::new([7; 32]), ChaCha20::new([7; 32]));
        let pa: Vec<u64> = (0..100).map(|_| a.priority()).collect();
        let pb: Vec<u64> = (0..100).map(|_| b.priority()).collect();
        assert_eq!(pa, pb);
    }

    #[test]
    fn different_seeds_give_different_priorities() {
        assert_ne!(
            ChaCha20::new([1; 32]).priority(),
            ChaCha20::new([2; 32]).priority()
        );
    }

    #[test]
    fn priorities_cross_block_boundaries_without_repeating() {
        let mut rng = ChaCha20::new([3; 32]);
        let ps: Vec<u64> = (0..64).map(|_| rng.priority()).collect();
        let mut dedup = ps.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), ps.len());
    }

    proptest::proptest! {
        #[test]
        fn priority_is_never_nil_or_leader(seed: [u8; 32]) {
            let mut rng = ChaCha20::new(seed);
            for _ in 0..32 {
                let p = rng.priority();
                proptest::prop_assert!(p != 0 && p != LEADER_PRIORITY);
            }
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
