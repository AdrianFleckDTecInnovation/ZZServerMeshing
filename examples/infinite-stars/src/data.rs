use super::{Chunk, CHUNK};
use std::{collections::VecDeque, sync::Mutex};
use zz_mesh::*;
pub const VOXELS: u32 = 301;
pub const BLUEPRINT: u32 = 302;
fn voxels(c: &Chunk) -> Result<Vec<u8>> {
    if c.generator != 1 {
        return Err(invalid("unsupported voxel generator version"));
    }
    let mut bytes = vec![0; 4096];
    let mut x = c.seed ^ c.part.wrapping_mul(0x9e3779b97f4a7c15);
    for (i, v) in bytes.iter_mut().enumerate() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        *v = if let Some(k) = c.placed.get(&(i as u16)) {
            u8::try_from(*k).map_err(|_| invalid("voxel material"))?
        } else if c.removed.contains(&(i as u16)) {
            0
        } else {
            1 | ((x >> 60) as u8) << 4
        };
    }
    Ok(bytes)
}
pub fn voxel_manifest(c: &Chunk) -> Result<Manifest> {
    let bytes = voxels(c)?;
    Ok(Manifest {
        key: DataKey {
            provider: VOXELS,
            object: c.planet,
            part: c.part,
        },
        revision: c.voxel_revision,
        basis: Basis::Procedural {
            seed: c.seed,
            generator: c.generator,
            config: digest(b"16x16x16;millimetres=1000"),
            registry: digest(b"air=0;stone=1"),
            content: digest(b"stone-texture-1"),
        },
        hash: digest(&bytes),
        length: bytes.len() as u64,
        dependencies: vec![],
    })
}
pub struct VoxelProvider {
    db: Store,
    history: Mutex<VecDeque<(Manifest, Vec<u8>)>>,
}
impl VoxelProvider {
    pub fn new(db: Store) -> Self {
        Self {
            db,
            history: Default::default(),
        }
    }
    fn current(&self, k: DataKey) -> Result<(Manifest, Vec<u8>)> {
        if k.provider != VOXELS {
            return Err(invalid("voxel provider ID"));
        }
        let Lookup::Present(r) = self.db.lookup(k.object.child(k.part))? else {
            return Err(Error::Missing("chunk".into()));
        };
        let c: Chunk = r.get(CHUNK, 1)?;
        let m = voxel_manifest(&c)?;
        if m.key != k {
            return Err(invalid("chunk address mismatch"));
        }
        let data = voxels(&c)?;
        Ok((m, data))
    }
}
impl DataProvider for VoxelProvider {
    fn schema(&self) -> u32 {
        VOXELS
    }
    fn describe(&self, key: DataKey) -> Result<Manifest> {
        Ok(self.current(key)?.0)
    }
    fn snapshot(&self, m: &Manifest) -> Result<Vec<u8>> {
        let mut h = self.history.lock().unwrap();
        if let Some((_, b)) = h.iter().find(|(v, _)| v == m) {
            return Ok(b.clone());
        }
        let (current, b) = self.current(m.key)?;
        if current != *m {
            return Err(Error::Missing("voxel history; refresh manifest".into()));
        }
        if h.len() >= 8 {
            h.pop_front();
        }
        h.push_back((current, b.clone()));
        Ok(b)
    }
    fn delta(&self, old: &Manifest, m: &Manifest) -> Result<Option<Patch>> {
        let previous = self
            .history
            .lock()
            .unwrap()
            .iter()
            .find(|(a, _)| a == old)
            .map(|(_, b)| b.clone());
        previous
            .map(|b| self.snapshot(m).map(|n| Patch::between(old, &b, &n)))
            .transpose()
    }
    fn prepare(&self, m: &Manifest, bytes: &[u8]) -> Result<()> {
        if bytes.len() != 4096 || m.key.provider != VOXELS || digest(bytes) != m.hash {
            return Err(invalid("invalid chunk artifact"));
        }
        Ok(())
    }
}
pub struct BlueprintProvider;
const HULL: &[u8] = b"IS-HULL\x01\x03\x02\x01\x03\x04\x01\x01\x02";
impl BlueprintProvider {
    pub fn manifest() -> Manifest {
        let h = digest(HULL);
        Manifest {
            key: DataKey {
                provider: BLUEPRINT,
                object: Id::numbered(901),
                part: 0,
            },
            revision: 1,
            basis: Basis::Artifact(h),
            hash: h,
            length: HULL.len() as u64,
            dependencies: vec![],
        }
    }
}
impl DataProvider for BlueprintProvider {
    fn schema(&self) -> u32 {
        BLUEPRINT
    }
    fn describe(&self, k: DataKey) -> Result<Manifest> {
        let m = Self::manifest();
        if k != m.key {
            return Err(Error::Missing("hull blueprint".into()));
        }
        Ok(m)
    }
    fn snapshot(&self, m: &Manifest) -> Result<Vec<u8>> {
        if *m != Self::manifest() {
            return Err(invalid("unknown blueprint"));
        }
        Ok(HULL.to_vec())
    }
}
