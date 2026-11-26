use crate::*;
use serde::{Deserialize, Serialize};
/// Byte-valued voxel changes on an unchanged chunk shape, limited to 65535 cells.
/// Indices are ascending. Mask bit zero represents the lowest cell in a byte.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VoxelWrites {
    Indices(Vec<(u16, u8)>),
    Runs(Vec<(u16, u16, u8)>),
    Palette {
        mask: Vec<u8>,
        palette: Vec<u8>,
        bits: Vec<u8>,
    },
}
impl VoxelWrites {
    pub fn alternatives(old: &[u8], new: &[u8]) -> Result<[Self; 3]> {
        if old.len() != new.len() || new.len() > 65535 {
            return Err(invalid("voxel delta shape"));
        }
        let updates: Vec<_> = old
            .iter()
            .zip(new)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, (_, &b))| (i as u16, b))
            .collect();
        let mut runs: Vec<(u16, u16, u8)> = vec![];
        let mut palette = vec![];
        let mut mask = vec![0; new.len().div_ceil(8)];
        let mut symbols = vec![];
        for &(i, v) in &updates {
            if let Some((start, n, last)) = runs
                .last_mut()
                .filter(|(s, n, p)| *p == v && u32::from(*s) + u32::from(*n) == u32::from(i))
            {
                let _ = (start, last);
                *n += 1;
            } else {
                runs.push((i, 1, v));
            }
            mask[i as usize / 8] |= 1 << (i % 8);
            let symbol = palette.iter().position(|p| *p == v).unwrap_or_else(|| {
                palette.push(v);
                palette.len() - 1
            });
            symbols.push(symbol);
        }
        let width = usize::BITS - palette.len().saturating_sub(1).leading_zeros();
        let mut bits = vec![0; (symbols.len() * width as usize).div_ceil(8)];
        for (k, s) in symbols.into_iter().enumerate() {
            for n in 0..width as usize {
                if s & (1 << n) != 0 {
                    let b = k * width as usize + n;
                    bits[b / 8] |= 1 << (b % 8);
                }
            }
        }
        Ok([
            Self::Indices(updates),
            Self::Runs(runs),
            Self::Palette {
                mask,
                palette,
                bits,
            },
        ])
    }
    pub fn apply(&self, data: &mut [u8]) -> Result<()> {
        match self {
            Self::Indices(rows) => {
                let mut last = None;
                for &(i, v) in rows {
                    if last.is_some_and(|l| l >= i) {
                        return Err(invalid("unsorted voxel indices"));
                    }
                    *data
                        .get_mut(i as usize)
                        .ok_or_else(|| invalid("voxel index"))? = v;
                    last = Some(i);
                }
            }
            Self::Runs(rows) => {
                let mut last = 0;
                for &(i, n, v) in rows {
                    let start = i as usize;
                    let end = start + n as usize;
                    if start < last || n == 0 {
                        return Err(invalid("overlapping or empty voxel ranges"));
                    }
                    data.get_mut(start..end)
                        .ok_or_else(|| invalid("voxel range"))?
                        .fill(v);
                    last = end;
                }
            }
            Self::Palette {
                mask,
                palette,
                bits,
            } => {
                if mask.len() != data.len().div_ceil(8) || palette.len() > 256 {
                    return Err(invalid("voxel palette shape"));
                }
                if data.len() % 8 != 0 && mask.last().is_some_and(|b| *b >> (data.len() % 8) != 0) {
                    return Err(invalid("mask padding"));
                }
                let count: usize = mask.iter().map(|v| v.count_ones() as usize).sum();
                let width =
                    (usize::BITS - palette.len().saturating_sub(1).leading_zeros()) as usize;
                if bits.len() != (count * width).div_ceil(8) || count > 0 && palette.is_empty() {
                    return Err(invalid("packed voxel length"));
                }
                let mut bit = 0;
                for (i, v) in data.iter_mut().enumerate() {
                    if mask[i / 8] & (1 << (i % 8)) != 0 {
                        let mut k = 0;
                        for shift in 0..width {
                            k |= usize::from((bits[bit / 8] >> (bit % 8)) & 1) << shift;
                            bit += 1;
                        }
                        *v = *palette
                            .get(k)
                            .ok_or_else(|| invalid("voxel palette symbol"))?;
                    }
                }
            }
        }
        Ok(())
    }
}
