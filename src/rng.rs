//! A tiny deterministic pseudo random generator (splitmix64).
//!
//! Determinism is a hard requirement of the correctness gates: the same seed
//! must reproduce the same schedule, the same VM exits, and the same output.
//! The standard library ships no seedable PRNG, and this crate takes zero
//! external dependencies, so we carry our own. splitmix64 is small, well known,
//! and passes basic statistical tests, which is plenty for driving fuzz style
//! memory access patterns.

/// A seedable splitmix64 generator.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Create a generator from a seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Return the next 64 bit value and advance the state.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a value in the half open range `[0, bound)`.
    ///
    /// # Panics
    /// Panics if `bound` is zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "bound must be non zero");
        self.next_u64() % bound
    }

    /// Return true with probability one in `n`.
    ///
    /// # Panics
    /// Panics if `n` is zero.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_diverges() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        // splitmix64 first outputs differ for these seeds.
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_is_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
        }
    }
}
