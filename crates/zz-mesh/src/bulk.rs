use crate::*;
use bevy_ecs::prelude::Resource;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread,
};

pub const BLOCK: usize = 32 * 1024;
pub const MAX_BLOB: usize = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DataKey {
    pub provider: u32,
    pub object: Id,
    pub part: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Basis {
    Artifact(Digest),
    Procedural {
        seed: u64,
        generator: u32,
        config: Digest,
        registry: Digest,
        content: Digest,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub key: DataKey,
    pub revision: u64,
    pub basis: Basis,
    pub hash: Digest,
    pub length: u64,
    pub dependencies: Vec<DataKey>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub key: DataKey,
    pub from: u64,
    pub base: Digest,
    pub offset: u32,
    pub remove: u32,
    pub insert: Vec<u8>,
    pub voxels: Option<VoxelWrites>,
}
impl Patch {
    pub fn between(old: &Manifest, previous: &[u8], next: &[u8]) -> Self {
        let prefix = previous
            .iter()
            .zip(next)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = previous[prefix..]
            .iter()
            .rev()
            .zip(next[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let mut patch = Self {
            key: old.key,
            from: old.revision,
            base: old.hash,
            offset: prefix as u32,
            remove: (previous.len() - prefix - suffix) as u32,
            insert: next[prefix..next.len() - suffix].to_vec(),
            voxels: None,
        };
        if let Ok(choices) = VoxelWrites::alternatives(previous, next) {
            if let Some((n, v)) = choices
                .into_iter()
                .filter_map(|v| encode(&v).ok().map(|b| (b.len(), v)))
                .min_by_key(|(n, _)| *n)
            {
                if n < patch.insert.len() {
                    patch.voxels = Some(v);
                    patch.offset = 0;
                    patch.remove = 0;
                    patch.insert.clear();
                }
            }
        }
        patch
    }
    pub fn apply(&self, old: &Manifest, bytes: &[u8], target: &Manifest) -> Result<Vec<u8>> {
        if old.key != self.key
            || target.key != old.key
            || old.revision != self.from
            || old.hash != self.base
            || old.basis != target.basis
            || target.revision <= old.revision
            || digest(bytes) != self.base
        {
            return Err(invalid("delta baseline mismatch"));
        }
        if let Some(v) = &self.voxels {
            if self.offset != 0
                || self.remove != 0
                || !self.insert.is_empty()
                || target.length != bytes.len() as u64
            {
                return Err(invalid("mixed delta formats"));
            }
            let mut out = bytes.to_vec();
            v.apply(&mut out)?;
            verify(target, &out)?;
            return Ok(out);
        }
        let start = self.offset as usize;
        let end = start
            .checked_add(self.remove as usize)
            .ok_or_else(|| invalid("delta range"))?;
        if end > bytes.len()
            || bytes.len() - self.remove as usize + self.insert.len() != target.length as usize
            || target.length > MAX_BLOB as u64
        {
            return Err(invalid("invalid delta range or target length"));
        }
        let mut out = Vec::with_capacity(target.length as usize);
        out.extend_from_slice(&bytes[..start]);
        out.extend_from_slice(&self.insert);
        out.extend_from_slice(&bytes[end..]);
        verify(target, &out)?;
        Ok(out)
    }
}
fn verify(m: &Manifest, data: &[u8]) -> Result<()> {
    if data.len() as u64 != m.length || m.length > MAX_BLOB as u64 || digest(data) != m.hash {
        return Err(invalid("content length or SHA-256 mismatch"));
    }
    Ok(())
}

pub trait DataProvider: Send + Sync + 'static {
    fn schema(&self) -> u32;
    fn describe(&self, key: DataKey) -> Result<Manifest>;
    fn available(&self, m: &Manifest) -> bool {
        self.describe(m.key).is_ok_and(|x| x == *m)
    }
    fn snapshot(&self, m: &Manifest) -> Result<Vec<u8>>;
    fn read_block(&self, m: &Manifest, offset: u64, limit: u32) -> Result<Vec<u8>> {
        let data = self.snapshot(m)?;
        verify(m, &data)?;
        let start = usize::try_from(offset).map_err(|_| invalid("bulk offset"))?;
        if start > data.len() || limit as usize > BLOCK {
            return Err(invalid("bulk block range"));
        }
        Ok(data[start..(start + limit as usize).min(data.len())].to_vec())
    }
    fn delta(&self, _old: &Manifest, _next: &Manifest) -> Result<Option<Patch>> {
        Ok(None)
    }
    /// Validate format and reconstruct required derived data before acknowledging readiness.
    fn prepare(&self, m: &Manifest, bytes: &[u8]) -> Result<()> {
        verify(m, bytes)
    }
}
#[derive(Default, Resource)]
pub struct Providers(BTreeMap<u32, Arc<dyn DataProvider>>);
impl Providers {
    pub fn register(&mut self, p: Arc<dyn DataProvider>) -> Result<()> {
        let id = p.schema();
        if id == 0 || self.0.contains_key(&id) {
            return Err(invalid(format!("duplicate provider {id}")));
        }
        self.0.insert(id, p);
        Ok(())
    }
    pub fn get(&self, id: u32) -> Result<Arc<dyn DataProvider>> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::Missing(format!("provider {id}")))
    }
}
pub struct BlobPin {
    _data: Arc<[u8]>,
    count: Arc<AtomicUsize>,
}
impl Drop for BlobPin {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Clone, Resource)]
pub struct BlobCache {
    limit: usize,
    used: usize,
    blobs: BTreeMap<Digest, Arc<[u8]>>,
    order: VecDeque<Digest>,
    versions: BTreeMap<DataKey, Manifest>,
    pub hits: u64,
    pub payload: u64,
    pins: BTreeMap<Digest, Arc<AtomicUsize>>,
}
impl Default for BlobCache {
    fn default() -> Self {
        Self::new(MAX_BLOB)
    }
}
impl BlobCache {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: 0,
            blobs: BTreeMap::new(),
            order: VecDeque::new(),
            versions: BTreeMap::new(),
            hits: 0,
            payload: 0,
            pins: Default::default(),
        }
    }
    pub fn bytes(&self) -> usize {
        self.used
    }
    pub fn get(&self, m: &Manifest) -> Option<Arc<[u8]>> {
        self.blobs
            .get(&m.hash)
            .filter(|x| x.len() as u64 == m.length)
            .cloned()
    }
    pub fn pin(&self, m: &Manifest) -> Result<BlobPin> {
        let data = self
            .get(m)
            .ok_or_else(|| Error::Missing("verified blob".into()))?;
        let count = self.pins[&m.hash].clone();
        count.fetch_add(1, Ordering::AcqRel);
        Ok(BlobPin { _data: data, count })
    }
    pub fn baseline(&self, k: DataKey) -> Option<&Manifest> {
        self.versions.get(&k)
    }
    pub fn ready(&self, m: &Manifest) -> bool {
        self.versions.get(&m.key) == Some(m) && self.blobs.contains_key(&m.hash)
    }
    fn remember(&mut self, m: &Manifest) -> Result<()> {
        if !self.versions.contains_key(&m.key) && self.versions.len() >= 4096 {
            return Err(Error::Busy("cached descriptors"));
        }
        self.versions.insert(m.key, m.clone());
        Ok(())
    }
    pub fn install(&mut self, m: &Manifest, data: Vec<u8>, p: &dyn DataProvider) -> Result<()> {
        if data.len() > self.limit {
            return Err(Error::Busy("cache bytes"));
        }
        verify(m, &data)?;
        p.prepare(m, &data)?;
        if self.blobs.contains_key(&m.hash) {
            return self.remember(m);
        }
        while self.used + data.len() > self.limit || self.blobs.len() >= 4096 {
            let pos = self
                .order
                .iter()
                .position(|h| self.pins[h].load(Ordering::Acquire) == 0)
                .ok_or(Error::Busy("all cache entries pinned"))?;
            let h = self.order.remove(pos).unwrap();
            self.used -= self.blobs.remove(&h).unwrap().len();
            self.pins.remove(&h);
            self.versions.retain(|_, v| v.hash != h);
        }
        self.remember(m)?;
        self.order.push_back(m.hash);
        self.used += data.len();
        self.blobs.insert(m.hash, data.into());
        self.pins.insert(m.hash, Arc::default());
        Ok(())
    }
    pub fn fetch(&mut self, m: &Manifest, p: &dyn DataProvider) -> Result<usize> {
        self.fetch_bounded(m, p, usize::MAX)
    }
    pub fn fetch_bounded(
        &mut self,
        m: &Manifest,
        p: &dyn DataProvider,
        budget: usize,
    ) -> Result<usize> {
        if p.schema() != m.key.provider || m.length > self.limit.min(MAX_BLOB) as u64 {
            return Err(invalid("provider or byte limit"));
        }
        if let Some(bytes) = self.get(m) {
            p.prepare(m, &bytes)?;
            self.remember(m)?;
            self.hits += 1;
            return Ok(0);
        }
        if let Some(old) = self.versions.get(&m.key) {
            if old.basis == m.basis && old.revision < m.revision {
                if let Some(patch) = p.delta(old, m)? {
                    let src = self.get(old).ok_or_else(|| invalid("evicted baseline"))?;
                    let cost = encode(&patch)?.len();
                    if cost < m.length as usize && cost <= budget {
                        let bytes = patch.apply(old, &src, m)?;
                        drop(src);
                        self.install(m, bytes, p)?;
                        self.payload += cost as u64;
                        return Ok(cost);
                    }
                }
            }
        }
        if m.length > budget as u64 {
            return Err(Error::Busy("migration byte budget"));
        }
        let data = p.snapshot(m)?;
        let n = data.len();
        self.install(m, data, p)?;
        self.payload += n as u64;
        Ok(n)
    }
}
/// An interrupted download retains exactly the verified contiguous prefix length in this session.
pub struct Download {
    pub manifest: Manifest,
    bytes: Vec<u8>,
    limit: usize,
}
impl Download {
    pub fn new(manifest: Manifest, limit: usize) -> Result<Self> {
        if manifest.length > limit.min(MAX_BLOB) as u64 {
            return Err(Error::Busy("staging budget"));
        }
        Ok(Self {
            manifest,
            bytes: Vec::new(),
            limit,
        })
    }
    pub fn offset(&self) -> usize {
        self.bytes.len()
    }
    pub fn append(&mut self, offset: usize, chunk: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(chunk.len())
            .ok_or_else(|| invalid("chunk range"))?;
        if chunk.len() > BLOCK || end > self.limit || end > self.manifest.length as usize {
            return Err(Error::Busy("bulk block"));
        }
        if offset < self.offset() {
            if self.bytes.get(offset..end) == Some(chunk) {
                return Ok(());
            }
            return Err(invalid("conflicting duplicate block"));
        }
        if offset != self.offset() {
            return Err(invalid("resume at the acknowledged offset"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }
    pub fn finish(self, cache: &mut BlobCache, p: &dyn DataProvider) -> Result<()> {
        cache.install(&self.manifest, self.bytes, p)
    }
}
type BulkJob = Box<dyn FnOnce() + Send>;
pub struct BulkExecutor {
    tx: SyncSender<BulkJob>,
}
impl BulkExecutor {
    pub fn new(queue: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<BulkJob>(queue);
        thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                job()
            }
        });
        Self { tx }
    }
    pub fn submit<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<Receiver<Result<T>>> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Box::new(move || {
                let _ = tx.send(work());
            }))
            .map_err(|e| match e {
                TrySendError::Full(_) => Error::Busy("bulk work queue"),
                TrySendError::Disconnected(_) => invalid("bulk worker stopped"),
            })?;
        Ok(rx)
    }
}
