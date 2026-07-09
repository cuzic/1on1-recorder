//! バックオフのjitter用。外部の`rand`クレートに頼らない決定的PRNG
//! (spike-03-timeline-driftと同じxorshift64*実装)。

use std::sync::atomic::{AtomicU64, Ordering};

pub struct AtomicRng(AtomicU64);

impl AtomicRng {
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(seed | 1))
    }

    pub fn next_f64(&self) -> f64 {
        loop {
            let x = self.0.load(Ordering::Relaxed);
            let mut y = x;
            y ^= y >> 12;
            y ^= y << 25;
            y ^= y >> 27;
            if self
                .0
                .compare_exchange(x, y, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let v = y.wrapping_mul(0x2545_F491_4F6C_DD1D);
                return (v >> 11) as f64 / (1u64 << 53) as f64;
            }
        }
    }

    pub fn next_bool_with_probability(&self, p: f64) -> bool {
        self.next_f64() < p
    }
}
