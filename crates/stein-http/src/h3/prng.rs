//! Seeded input damage for the h3 untrusted-input tests.

/// SplitMix64 (Steele, Lea, Flood, OOPSLA 2014).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// Noise up to `max` bytes one time in five, else `seed` flipped, cut, grown or spliced.
    pub fn damage(&mut self, seed: &[u8], max: usize) -> Vec<u8> {
        if self.below(5) == 0 {
            let n = self.below(max as u64 + 1) as usize;
            return (0..n).map(|_| self.next() as u8).collect();
        }
        let mut data = seed.to_vec();
        for _ in 0..1 + self.below(4) {
            let at = self.below(data.len() as u64 + 1) as usize;
            match self.below(4) {
                0 if at < data.len() => data[at] ^= 1 << self.below(8),
                1 => data.truncate(at),
                2 => data.insert(at, self.next() as u8),
                _ if at < data.len() => data[at] = self.next() as u8,
                _ => data.push(self.next() as u8),
            }
        }
        data
    }
}
