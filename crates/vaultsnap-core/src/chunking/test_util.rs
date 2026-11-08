//! Deterministic test data.

/// `len` bytes from an xorshift64 generator: reproducible, and random enough for chunking.
///
/// Different seeds give different data. (The state must be odd, so seeding with `seed | 1` alone
/// would make seeds 2 and 3, 4 and 5, and so on identical; the seed is first multiplied by an odd
/// constant, which is a bijection on `u64`, so no two small seeds can meet.)
pub(crate) fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn different_seeds_give_different_data_and_the_same_seed_the_same() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..1000 {
            assert!(
                seen.insert(pseudo_random(64, seed)),
                "seed {seed} repeats an earlier seed"
            );
        }
        assert_eq!(pseudo_random(100, 7), pseudo_random(100, 7));
        assert_eq!(pseudo_random(10, 7), pseudo_random(100, 7)[..10]);
    }
}
