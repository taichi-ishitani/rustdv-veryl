//! The random number generator of objects and of the per-thread seed
//! source: SplitMix64, the algorithm of rustdv's `Rng`.

use num_bigint::{BigInt, BigUint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    pub fn state(&self) -> u64 {
        self.state
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        // Rejection sampling avoids the modulo bias.
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }

    /// A value in `0..n` for a positive `n` of any size.
    pub fn below_big(&mut self, n: &BigInt) -> BigInt {
        let n = n.to_biguint().expect("positive bound");
        let bits = n.bits();
        let words = bits.div_ceil(64) as usize;
        loop {
            let mut digits: Vec<u64> = (0..words).map(|_| self.next_u64()).collect();
            let extra = (words as u64) * 64 - bits;
            if let Some(top) = digits.last_mut()
                && extra > 0
            {
                *top &= u64::MAX >> extra;
            }
            let v = BigUint::from_slice(
                &digits
                    .iter()
                    .flat_map(|d| [*d as u32, (*d >> 32) as u32])
                    .collect::<Vec<u32>>(),
            );
            if v < n {
                return BigInt::from(v);
            }
        }
    }
}
