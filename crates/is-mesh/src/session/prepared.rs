//! Worker-local installation of authoritative control observations made on
//! the bounded, mutually authenticated socket workers. No I/O runs in a tick.
use super::*;
use std::time::{Duration, Instant};

// Discard delayed storage results. A negative fence remains longer than any
// result which could have started before the revocation transaction.
const OBSERVATION_LIMIT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Pending,
    Ready,
    Revoked,
}

#[derive(Copy, Clone)]
struct Observation {
    group: Id,
    owner: Authority,
    state: ControlState,
    phase: Phase,
    deadline: Instant,
    began: Instant,
}
impl Observation {
    fn live(&self, now: Instant, utc: u64) -> bool {
        self.phase == Phase::Ready && now < self.deadline && utc < self.state.expires_ms
    }
}

/// Share this bounded resource between a MeshSessionPlugin world and its
/// WorkerEndpoint. The gateway waits for explicit installation of fence changes.
#[derive(Resource, Clone)]
pub struct PreparedSessionControls {
    entries: Arc<Mutex<BTreeMap<Id, Observation>>>,
    capacity: usize,
}
impl PreparedSessionControls {
    /// Run one bounded game mutation under the exact worker-local authority,
    /// synchronized control lease and command deadline. An opaque command's
    /// replay/sequence policy belongs to its registered dispatcher. The game
    /// must obtain `object` on the actual entity with `With<Simulated>`.
    pub fn apply_command<T>(
        &self,
        mesh: &MeshState,
        object: &Managed,
        binding: ControlBinding,
        expires_unix_ms: u64,
        apply: impl FnOnce() -> T,
    ) -> Result<T> {
        let entries = self
            .entries
            .try_lock()
            .map_err(|_| Error::Busy("worker control gate"))?;
        let permit = entries.get(&object.id).ok_or(Error::Fenced)?;
        let utc = control_now()?;
        if !permit.live(Instant::now(), utc)
            || utc >= expires_unix_ms
            || permit.group != object.group
            || mesh.authority(object.group) != Some(permit.owner)
            || permit.state.binding != binding
            || binding.object != object.id
        {
            return Err(Error::Fenced);
        }
        Ok(apply())
    }
    /// Consume one sample on its actual authoritative entity. Use in the game
    /// system that mutates the object, with a With<Simulated> query. The short
    /// critical section covers the mutation and its migratable receipt. Keep
    /// the closure synchronous and bounded. An expired sample has no effect.
    pub fn apply_input(
        &self,
        mesh: &MeshState,
        object: &Managed,
        binding: ControlBinding,
        sample: &ValidatedInput,
        receipt: &mut InputCursor,
        apply: impl FnOnce([i16; 3]),
    ) -> bool {
        if sample.binding != binding
            || receipt.binding == Some(binding) && receipt.through >= sample.sequence
        {
            return false;
        }
        self.apply_command(mesh, object, binding, sample.expires_ms, || {
            apply(sample.axes);
            receipt.binding = Some(binding);
            receipt.through = sample.sequence;
        })
        .is_ok()
    }
    pub fn install(world: &mut World) -> Result<Self> {
        let ingress = world
            .get_resource::<SessionIngress>()
            .ok_or_else(|| invalid("missing MeshSessionPlugin"))?;
        let prepared = Self {
            entries: Default::default(),
            capacity: ingress.capacity(),
        };
        ingress
            .0
            .prepared
            .set(prepared.clone())
            .map_err(|_| invalid("worker control mode already installed"))?;
        world.insert_resource(prepared.clone());
        world.init_resource::<PreparedAuthority>();
        Ok(prepared)
    }
    /// Only called by the mutually authenticated socket endpoint, outside ECS.
    pub(crate) fn observe(
        &self,
        store: &Store,
        worker: Worker,
        bindings: &[ControlBinding],
        require_ready: bool,
    ) -> Result<()> {
        for result in self.observe_batch(store, worker, bindings, require_ready)? {
            result?;
        }
        Ok(())
    }
    pub(crate) fn observe_independent(
        &self,
        store: &Store,
        worker: Worker,
        bindings: &[ControlBinding],
    ) -> Result<Vec<bool>> {
        Ok(self
            .observe_batch(store, worker, bindings, true)?
            .into_iter()
            .map(|r| r.is_ok())
            .collect())
    }
    fn observe_batch(
        &self,
        store: &Store,
        worker: Worker,
        bindings: &[ControlBinding],
        require_ready: bool,
    ) -> Result<Vec<Result<()>>> {
        if bindings.len() > MAX_TARGETS {
            return Err(Error::Busy("worker control batch"));
        }
        let began = Instant::now();
        let ids: Vec<_> = bindings.iter().map(|b| b.object).collect();
        let observed = store.control_observations(worker, &ids)?;
        let utc = control_now()?;
        let mut entries = self.entries.lock().map_err(|_| Error::Fenced)?;
        let now = Instant::now();
        if now.duration_since(began) >= OBSERVATION_LIMIT {
            return Err(Error::Busy("worker control observation timeout"));
        }
        Ok(bindings
            .iter()
            .map(|wanted| {
                let Some(&(group, generation, state, synchronized)) = observed.get(&wanted.object)
                else {
                    if let Some(old) = entries.get_mut(&wanted.object) {
                        if old.owner.worker == worker && old.began <= began {
                            old.phase = Phase::Pending;
                            old.began = began;
                            old.deadline = now + OBSERVATION_LIMIT;
                        }
                    }
                    return Err(Error::Fenced);
                };
                let owner = Authority { worker, generation };
                let phase = if state.closed || !state.active || state.expires_ms <= utc {
                    Phase::Revoked
                } else if synchronized {
                    Phase::Ready
                } else {
                    Phase::Pending
                };
                let key = (state.binding.epoch, owner.generation);
                if let Some(old) = entries.get(&wanted.object) {
                    let old_key = (old.state.binding.epoch, old.owner.generation);
                    if old_key > key
                        || (old_key == key
                            && (old.began > began
                                || (old.phase == Phase::Revoked && phase != Phase::Revoked)))
                    {
                        return Err(Error::Fenced);
                    }
                } else if entries.len() >= self.capacity {
                    entries.retain(|_, v| v.deadline > now);
                    if entries.len() >= self.capacity {
                        return Err(Error::Busy("worker control fence capacity"));
                    }
                }
                let deadline = if phase == Phase::Ready {
                    began
                        + Duration::from_millis(
                            state
                                .expires_ms
                                .saturating_sub(utc)
                                .min(MAX_CONTROL_LEASE_MS),
                        )
                } else {
                    now + OBSERVATION_LIMIT
                };
                entries.insert(
                    wanted.object,
                    Observation {
                        group,
                        owner,
                        state,
                        phase,
                        deadline,
                        began,
                    },
                );
                if state.binding != *wanted {
                    return Err(Error::Fenced);
                }
                if require_ready && phase != Phase::Ready {
                    return Err(Error::Denied);
                }
                Ok(())
            })
            .collect())
    }
    pub(super) fn accepts(&self, binding: ControlBinding) -> Result<bool> {
        let entries = self
            .entries
            .try_lock()
            .map_err(|_| Error::Busy("worker control refresh"))?;
        let utc = control_now()?;
        Ok(entries
            .get(&binding.object)
            .is_some_and(|v| v.state.binding == binding && v.live(Instant::now(), utc)))
    }
    pub(super) fn states(
        &self,
        worker: Worker,
        objects: &[(Id, Id, u64)],
    ) -> Result<Vec<Option<ControlState>>> {
        let now = Instant::now();
        let utc = control_now()?;
        let entries = self
            .entries
            .try_lock()
            .map_err(|_| Error::Busy("worker control refresh"))?;
        Ok(objects
            .iter()
            .map(|(id, g, generation)| {
                entries
                    .get(id)
                    .filter(|v| {
                        v.group == *g
                            && v.owner.worker == worker
                            && v.owner.generation == *generation
                            && v.live(now, utc)
                    })
                    .map(|v| v.state)
            })
            .collect())
    }
}

impl Store {
    /// One consistent indexed read for binding, group, generation, worker
    /// incarnation, freeze and sync. Only a validated server worker reaches it.
    pub(crate) fn control_observations(
        &self,
        worker: Worker,
        ids: &[Id],
    ) -> Result<BTreeMap<Id, (Id, u64, ControlState, bool)>> {
        if ids.len() > MAX_TARGETS {
            return Err(Error::Busy("worker control observations"));
        }
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::ObserveControls(
                worker,
                ids.to_vec(),
            ));
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT b.object,n.unit,u.gen,b.state,(u.frozen IS NULL AND p.object IS NULL)
            FROM control_bindings b JOIN names n ON n.id=b.object JOIN units u ON u.id=n.unit
            JOIN workers w ON w.slot=u.slot AND w.incarnation=u.inc
            LEFT JOIN control_sync_pending p ON p.object=b.object
            WHERE u.slot=? AND u.inc=? AND b.object IN ({placeholders})"
        );
        let mut parameters: Vec<&dyn rusqlite::ToSql> = vec![&worker.slot, &worker.incarnation];
        parameters.extend(ids.iter().map(|id| id as &dyn rusqlite::ToSql));
        let db = self.lock()?;
        let mut query = db.prepare_cached(&sql)?;
        let rows = query.query_map(parameters.as_slice(), |r| {
            Ok((
                r.get::<_, Id>(0)?,
                r.get::<_, Id>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut found = BTreeMap::new();
        for row in rows {
            let (id, group, generation, raw, synced) = row?;
            found.insert(id, (group, generation, decode(&raw)?, synced));
        }
        Ok(found)
    }
}
