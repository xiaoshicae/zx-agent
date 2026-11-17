//! Randomness for spreading retries and reconnections: the standard library's randomly keyed
//! hasher is enough for that, without a random number generator among the dependencies.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// A random number: a constant hashed under a newly keyed hasher.
pub fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0x5eed);
    hasher.finish()
}

/// A random number in `[0, 1)`.
pub fn random_fraction() -> f64 {
    // The top 53 bits make an exact double in [0, 1).
    (random_u64() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_are_below_one_and_not_negative() {
        for _ in 0..1000 {
            let fraction = random_fraction();
            assert!((0.0..1.0).contains(&fraction), "{fraction}");
        }
    }
}
