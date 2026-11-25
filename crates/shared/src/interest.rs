//! Fixed-size discovery hints for the browser demo's XZ peer protocol.
//! A hit can trigger a cold bootstrap. The client visibility test stays exact.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestMask([[u64; 16]; 8]);

#[derive(Clone, Copy)]
pub struct CellBits([usize; 2]);

impl InterestMask {
    pub fn locations(x: i32, z: i32) -> CellBits {
        // SplitMix64's finalizer disperses signed grid coordinates across 8192 bits.
        let mut h = (u64::from(x as u32) << 32) | u64::from(z as u32);
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
        h ^= h >> 31;
        CellBits([(h & 8191) as usize, ((h >> 32) & 8191) as usize])
    }
    pub fn observe(&mut self, x: i32, z: i32) {
        for i in Self::locations(x, z).0 {
            self.0[i / 1024][i / 64 % 16] |= 1 << (i % 64);
        }
    }
    pub fn may_contain(&self, bits: CellBits) -> bool {
        bits.0
            .into_iter()
            .all(|i| self.0[i / 1024][i / 64 % 16] & (1 << (i % 64)) != 0)
    }
}
