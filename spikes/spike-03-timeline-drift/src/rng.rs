//! 決定的な疑似乱数(xorshift64*)。外部の`rand`クレートに頼らず、
//! シードだけで完全に再現可能なテストにするための最小実装。

pub struct SimpleRng(u64);

impl SimpleRng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0.0, 1.0)の一様乱数。
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn next_bool_with_probability(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}
