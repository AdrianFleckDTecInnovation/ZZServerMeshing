//! Trusted server admission, independent of the public transport and identity provider.
use crate::*;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
mod prepared;
pub use prepared::PreparedSessionControls;

macro_rules! identity {
    ($name:ident) => {
        #[derive(
            Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub Id);
    };
}
identity!(PrincipalId);
identity!(SessionId);
identity!(ConnectionId);
identity!(SubscriberId);

/// A trusted gateway has verified the provider session. A takeover of a live
/// connection requires the same verified session and its expected prior epoch.
/// The authority allocates epochs and initially withholds worker input rights.
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct ControlAdmission {
    pub principal: PrincipalId,
    pub session: SessionId,
    pub object: Id,
    pub connection: ConnectionId,
    pub resume_epoch: Option<u64>,
    pub expires_ms: u64,
}

/// Maximum lifetime of a freshly verified game admission.
pub const MAX_CONTROL_LEASE_MS: u64 = 120_000;

/// Constructed by a trusted server after external authorization. Epochs strictly
/// increase per object, including reconnects; an exact retry is idempotent.
pub struct SessionGrant {
    pub principal: PrincipalId,
    pub session: SessionId,
    pub object: Id,
    pub control_epoch: u64,
    pub expires_at: SystemTime,
}
#[derive(Component, Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlBinding {
    pub principal: PrincipalId,
    pub session: SessionId,
    pub object: Id,
    pub epoch: u64,
    pub connection: ConnectionId,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlState {
    pub binding: ControlBinding,
    pub expires_ms: u64,
    pub subscriber: Option<SubscriberId>,
    pub active: bool,
    pub closed: bool,
}
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum ControlChange {
    Admit(ControlState),
    Suspend(ControlBinding),
    Close(ControlBinding),
    Renew(ControlBinding, u64),
    Subscribe(ControlBinding, Option<SubscriberId>),
}
impl ControlChange {
    fn binding(self) -> ControlBinding {
        match self {
            Self::Admit(s) => s.binding,
            Self::Suspend(b) | Self::Close(b) | Self::Renew(b, _) | Self::Subscribe(b, _) => b,
        }
    }
    fn apply(self, old: Option<ControlState>, now: u64) -> Result<ControlState> {
        let b = self.binding();
        if let Self::Admit(next) = self {
            if b.epoch == 0
                || next.expires_ms <= now
                || !next.active
                || next.closed
                || next.subscriber.is_some()
            {
                return Err(Error::Denied);
            }
            if let Some(previous) = old {
                if previous.binding == b
                    && previous.active
                    && !previous.closed
                    && previous.expires_ms > now
                {
                    return Ok(previous);
                }
                if previous.binding.epoch >= b.epoch {
                    return Err(Error::Fenced);
                }
            }
            return Ok(next);
        }
        let mut state = old.filter(|s| s.binding == b).ok_or(Error::Fenced)?;
        match self {
            Self::Close(_) => {
                state.active = false;
                state.closed = true;
                state.subscriber = None;
            }
            Self::Suspend(_) => {
                state.active = false;
                state.subscriber = None;
            }
            Self::Renew(_, expiry)
                if state.active
                    && !state.closed
                    && state.expires_ms > now
                    && expiry >= state.expires_ms =>
            {
                state.expires_ms = expiry
            }
            Self::Subscribe(_, id) if state.active && !state.closed && state.expires_ms > now => {
                state.subscriber = id
            }
            _ => return Err(Error::Denied),
        }
        Ok(state)
    }
}
fn millis(t: SystemTime) -> Result<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| d.as_millis().try_into().ok())
        .ok_or_else(|| invalid("session time outside epoch"))
}

// A backward adjustment of UTC must never extend an already-issued lease.
// Keep the greatest observed UTC offset from this process's monotonic clock.
pub(crate) fn control_now() -> Result<u64> {
    use std::sync::{atomic::AtomicU64, OnceLock};
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    static OFFSET: AtomicU64 = AtomicU64::new(0);
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis() as u64;
    Ok(conservative_utc(
        &OFFSET,
        elapsed,
        millis(SystemTime::now())?,
    ))
}
fn conservative_utc(offset: &std::sync::atomic::AtomicU64, elapsed: u64, utc: u64) -> u64 {
    let seen = utc.saturating_sub(elapsed);
    elapsed.saturating_add(
        offset
            .fetch_max(seen, std::sync::atomic::Ordering::Relaxed)
            .max(seen),
    )
}
#[cfg(test)]
#[test]
fn clock_rollback_does_not_extend_an_issued_worker_deadline() {
    let offset = std::sync::atomic::AtomicU64::new(0);
    assert_eq!(conservative_utc(&offset, 0, 1000), 1000);
    assert_eq!(conservative_utc(&offset, 10, 2010), 2010);
    let deadline = 2020;
    assert!(conservative_utc(&offset, 19, 1019) < deadline);
    assert!(conservative_utc(&offset, 20, 1020) >= deadline);
}
#[derive(Default)]
struct LocalControls {
    objects: BTreeMap<Id, ControlState>,
    closed: std::collections::BTreeSet<SessionId>,
    sessions: BTreeMap<SessionId, (PrincipalId, Id)>,
    subscribers: BTreeMap<SubscriberId, Id>,
}
#[derive(Clone)]
enum Controls {
    Local(Arc<Mutex<LocalControls>>, usize),
    Persistent(Store),
}
#[derive(Resource, Clone)]
pub struct SessionDirectory(Controls);
impl SessionDirectory {
    /// Share this instance between embedded adapters. Identity history is bounded.
    pub fn transient(capacity: std::num::NonZeroUsize) -> Self {
        Self(Controls::Local(Default::default(), capacity.get()))
    }
    /// The same authority volume arbitrates concurrent admissions from different workers.
    pub fn persistent(store: Store) -> Self {
        Self(Controls::Persistent(store))
    }
    fn change(&self, change: ControlChange) -> Result<ControlState> {
        match &self.0 {
            Controls::Persistent(store) => store.control_change(change),
            Controls::Local(table, limit) => {
                let mut table = table.lock().map_err(|_| Error::Fenced)?;
                let b = change.binding();
                if table.closed.contains(&b.session) && !matches!(change, ControlChange::Close(_)) {
                    return Err(Error::Denied);
                }
                let next = change.apply(table.objects.get(&b.object).copied(), control_now()?)?;
                if let Some(identity) = table.sessions.get(&b.session) {
                    if *identity != (b.principal, b.object) {
                        return Err(Error::Denied);
                    }
                } else if table.sessions.len() >= *limit {
                    return Err(Error::Busy("session identity capacity"));
                }
                if next
                    .subscriber
                    .is_some_and(|id| table.subscribers.get(&id).is_some_and(|v| *v != b.object))
                {
                    return Err(Error::Denied);
                }
                if let Some(id) = table.objects.get(&b.object).and_then(|s| s.subscriber) {
                    table.subscribers.remove(&id);
                }
                if let Some(id) = next.subscriber {
                    table.subscribers.insert(id, b.object);
                }
                if next.closed {
                    table.closed.insert(b.session);
                }
                table.sessions.insert(b.session, (b.principal, b.object));
                table.objects.insert(b.object, next);
                Ok(next)
            }
        }
    }
    pub fn admit(&self, grant: SessionGrant, connection: ConnectionId) -> Result<ControlBinding> {
        let binding = ControlBinding {
            principal: grant.principal,
            session: grant.session,
            object: grant.object,
            epoch: grant.control_epoch,
            connection,
        };
        self.change(ControlChange::Admit(ControlState {
            binding,
            expires_ms: millis(grant.expires_at)?,
            subscriber: None,
            active: true,
            closed: false,
        }))?;
        Ok(binding)
    }
    /// Deactivate steering and its private subscriber immediately at the authority.
    pub fn revoke(&self, binding: ControlBinding) -> Result<()> {
        self.suspend(binding)
    }
    pub fn suspend(&self, binding: ControlBinding) -> Result<()> {
        self.change(ControlChange::Suspend(binding)).map(drop)
    }
    pub fn close(&self, binding: ControlBinding) -> Result<()> {
        self.change(ControlChange::Close(binding)).map(drop)
    }
    pub fn renew(&self, binding: ControlBinding, expires_at: SystemTime) -> Result<()> {
        self.change(ControlChange::Renew(binding, millis(expires_at)?))
            .map(drop)
    }
    pub fn subscribe(
        &self,
        binding: ControlBinding,
        subscriber: Option<SubscriberId>,
    ) -> Result<()> {
        self.change(ControlChange::Subscribe(binding, subscriber))
            .map(drop)
    }
    pub fn states(&self, objects: &[Id]) -> Result<Vec<Option<ControlState>>> {
        if objects.len() > MAX_TARGETS {
            return Err(Error::Busy("control batch"));
        }
        let states: Vec<_> = match &self.0 {
            Controls::Persistent(store) => store.control_states(objects)?,
            Controls::Local(table, _) => {
                let table = table.lock().map_err(|_| Error::Fenced)?;
                objects
                    .iter()
                    .map(|id| table.objects.get(id).copied())
                    .collect()
            }
        };
        let now = control_now()?;
        Ok(states
            .into_iter()
            .map(|state| state.filter(|s| s.active && !s.closed && s.expires_ms > now))
            .collect())
    }
    pub fn authorized_subscriber(
        &self,
        binding: ControlBinding,
        subscriber: SubscriberId,
    ) -> Result<bool> {
        Ok(self.states(&[binding.object])?[0]
            .is_some_and(|s| s.binding == binding && s.subscriber == Some(subscriber)))
    }
    pub(crate) fn for_worker(
        &self,
        worker: Worker,
        objects: &[(Id, Id, u64)],
    ) -> Result<Vec<Option<ControlState>>> {
        match &self.0 {
            Controls::Persistent(store) => store.worker_controls(worker, objects),
            Controls::Local(_, _) => self.states(&objects.iter().map(|v| v.0).collect::<Vec<_>>()),
        }
    }
}

impl Store {
    /// Serialize simultaneous gateways inside the existing authority transaction.
    /// Replaying an attempt cannot resurrect a replaced or suspended connection.
    pub fn reserve_control(&self, request: ControlAdmission) -> Result<ControlBinding> {
        use rusqlite::{params, OptionalExtension};
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::ReserveControl(request));
        }
        self.write(|db| {
            let now = control_now()?;
            if request.expires_ms <= now
                || request.expires_ms > now.saturating_add(MAX_CONTROL_LEASE_MS)
            {
                return Err(Error::Denied);
            }
            let old = read_control(db, request.object)?;
            if let Some(s) = old {
                let b = s.binding;
                let live = s.active && !s.closed && s.expires_ms > now;
                let same_session = b.principal == request.principal && b.session == request.session;
                if b.connection == request.connection {
                    return if live && same_session {
                        Ok(b)
                    } else {
                        Err(Error::Fenced)
                    };
                }
                match request.resume_epoch {
                    Some(previous) if previous != b.epoch || !same_session || s.closed => {
                        return Err(Error::Fenced)
                    }
                    None if live => return Err(Error::Denied),
                    _ => (),
                }
            } else if request.resume_epoch.is_some() {
                return Err(Error::Fenced);
            }
            if db
                .query_row(
                    "SELECT 1 FROM closed_control_sessions WHERE session=?",
                    [request.session.0],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                return Err(Error::Denied);
            }
            db.execute("DELETE FROM control_attempts WHERE connection IN (
                SELECT connection FROM control_attempts WHERE expires_ms<=? ORDER BY expires_ms LIMIT 64)", [now])?;
            if db.execute(
                "INSERT OR IGNORE INTO control_attempts(connection,expires_ms) VALUES(?,?)",
                params![request.connection.0, request.expires_ms],
            )? != 1
            {
                return Err(Error::Fenced);
            }
            let epoch = old
                .map_or(0, |s| s.binding.epoch)
                .checked_add(1)
                .ok_or(Error::Fenced)?;
            let binding = ControlBinding {
                principal: request.principal,
                session: request.session,
                object: request.object,
                connection: request.connection,
                epoch,
            };
            let state = ControlState {
                binding,
                active: true,
                closed: false,
                expires_ms: request.expires_ms,
                subscriber: None,
            };
            write_control(db, state)?;
            db.execute(
                "INSERT OR IGNORE INTO control_sync_pending(object) VALUES(?)",
                [request.object],
            )?;
            Ok(binding)
        })
    }

    /// Release worker steering only after the trusted gateway validated the
    /// exact binding's application-level snapshot acknowledgement.
    pub fn confirm_control(&self, binding: ControlBinding) -> Result<()> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::ConfirmControl(binding));
        }
        self.write(|db| {
            let state = read_control(db, binding.object)?.ok_or(Error::Denied)?;
            if state.binding != binding
                || !state.active
                || state.closed
                || state.subscriber.is_none()
                || state.expires_ms <= control_now()?
            {
                return Err(Error::Fenced);
            }
            db.execute(
                "DELETE FROM control_sync_pending WHERE object=?",
                [binding.object],
            )?;
            Ok(())
        })
    }

    pub(crate) fn control_change(&self, change: ControlChange) -> Result<ControlState> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::ChangeControl(change));
        }
        self.write(|db| {
            let b = change.binding();
            if !matches!(change, ControlChange::Close(_))
                && db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM closed_control_sessions WHERE session=?)",
                    [b.session.0],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Err(Error::Denied);
            }
            let old = read_control(db, b.object)?;
            let next = change.apply(old, control_now()?)?;
            write_control(db, next)?;
            if matches!(change, ControlChange::Admit(_)) && old.is_none_or(|s| s.binding != b) {
                db.execute(
                    "DELETE FROM control_sync_pending WHERE object=?",
                    [b.object],
                )?;
            }
            Ok(next)
        })
    }
    pub(crate) fn control_states(&self, objects: &[Id]) -> Result<Vec<Option<ControlState>>> {
        use rusqlite::OptionalExtension;
        if objects.len() > MAX_TARGETS {
            return Err(Error::Busy("control batch"));
        }
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::ReadControls(objects.to_vec()));
        }
        let db = self.lock()?;
        let mut get = db.prepare_cached("SELECT state FROM control_bindings WHERE object=?")?;
        objects
            .iter()
            .map(|id| {
                let row: Option<Vec<u8>> = get.query_row([id], |r| r.get(0)).optional()?;
                row.map(|v| decode(&v)).transpose()
            })
            .collect()
    }
    pub(crate) fn worker_controls(
        &self,
        worker: Worker,
        objects: &[(Id, Id, u64)],
    ) -> Result<Vec<Option<ControlState>>> {
        if objects.len() > MAX_TARGETS {
            return Err(Error::Busy("worker control batch"));
        }
        if objects.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(remote) = self.remote_connection() {
            return remote.call(authority_store::Operation::WorkerControls(
                worker,
                objects.to_vec(),
            ));
        }
        // A single SQLite statement sees one grant snapshot for the entire input batch.
        // Materialize only the requested triples; the authority and object keys are indexed.
        let placeholders = std::iter::repeat_n("(?,?,?)", objects.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql=format!("WITH wanted(object,unit,gen) AS (VALUES {placeholders}) SELECT b.object,b.state FROM wanted v JOIN control_bindings b ON b.object=v.object JOIN names n ON n.id=b.object AND n.unit=v.unit JOIN units u ON u.id=n.unit AND u.gen=v.gen JOIN workers w ON w.slot=u.slot AND w.incarnation=u.inc WHERE u.slot=? AND u.inc=? AND u.frozen IS NULL AND NOT EXISTS(SELECT 1 FROM control_sync_pending p WHERE p.object=b.object)");
        let mut parameters: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(objects.len() * 3 + 2);
        for (object, group, generation) in objects {
            parameters.extend([object as &dyn rusqlite::ToSql, group, generation]);
        }
        parameters.extend([&worker.slot as &dyn rusqlite::ToSql, &worker.incarnation]);
        let db = self.lock()?;
        let mut query = db.prepare_cached(&sql)?;
        let rows = query.query_map(parameters.as_slice(), |r| {
            Ok((r.get::<_, Id>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let now = control_now()?;
        let mut result = std::collections::HashMap::with_capacity(objects.len());
        for row in rows {
            let (id, bytes) = row?;
            let state: ControlState = decode(&bytes)?;
            if state.active && !state.closed && state.expires_ms > now {
                result.insert(id, state);
            }
        }
        Ok(objects
            .iter()
            .map(|(id, _, _)| result.get(id).copied())
            .collect())
    }
}

fn read_control(db: &rusqlite::Connection, object: Id) -> Result<Option<ControlState>> {
    use rusqlite::OptionalExtension;
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT state FROM control_bindings WHERE object=?",
            [object],
            |r| r.get(0),
        )
        .optional()?;
    bytes.map(|v| decode(&v)).transpose()
}

fn write_control(db: &rusqlite::Connection, state: ControlState) -> Result<()> {
    use rusqlite::params;
    let b = state.binding;
    db.execute(
        "INSERT OR IGNORE INTO control_sessions(session,principal,object) VALUES(?,?,?)",
        params![b.session.0, b.principal.0, b.object],
    )?;
    let matches: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM control_sessions WHERE session=? AND principal=? AND object=?)",
        params![b.session.0,b.principal.0,b.object], |r| r.get(0))?;
    if !matches {
        return Err(Error::Denied);
    }
    if state.closed {
        db.execute(
            "INSERT OR IGNORE INTO closed_control_sessions(session) VALUES(?)",
            [b.session.0],
        )?;
    }
    db.execute("DELETE FROM control_subscribers WHERE object=?", [b.object])?;
    if let Some(id) = state.subscriber {
        if db.execute(
            "INSERT OR IGNORE INTO control_subscribers(subscriber,object) VALUES(?,?)",
            params![id.0, b.object],
        )? == 0
        {
            return Err(Error::Denied);
        }
    }
    db.execute("INSERT INTO control_bindings(object,state) VALUES(?,?) ON CONFLICT(object) DO UPDATE SET state=excluded.state", params![b.object,encode(&state)?])?;
    Ok(())
}

/// A latest-state input submitted by a trusted adapter. The consumer rechecks
/// the connection, control epoch and the actual local writer before gameplay.
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct SessionInput {
    pub binding: ControlBinding,
    pub sequence: u64,
    pub at_ms: u64,
    pub axes: [i16; 3],
}
#[derive(Component, Debug, Copy, Clone)]
pub struct ValidatedInput {
    pub sequence: u64,
    pub axes: [i16; 3],
    pub at_ms: u64,
    binding: ControlBinding,
    expires_ms: u64,
}
#[derive(Component, Default, Serialize, Deserialize)]
pub struct InputCursor {
    binding: Option<ControlBinding>,
    through: u64,
}
pub fn confirmed_session_input(world: &World, binding: ControlBinding) -> Result<InputAck> {
    let state = world.resource::<MeshState>();
    let e = *state.entities.get(&binding.object).ok_or(Error::Fenced)?;
    let group = world.get::<Managed>(e).ok_or(Error::Fenced)?.group;
    let a = state.authority(group).ok_or(Error::Fenced)?;
    if world.get::<Simulated>(e).is_none()
        || worker_states(world, a.worker, &[(binding.object, group, a.generation)])?[0]
            .is_none_or(|s| s.binding != binding)
    {
        return Err(Error::Denied);
    }
    Ok(InputAck {
        sequence: world
            .get::<InputCursor>(e)
            .filter(|c| c.binding == Some(binding))
            .map_or(0, |c| c.through),
        revision: state.version(binding.object),
    })
}
#[derive(Component)]
pub struct MeshReady;

#[derive(Clone, Copy)]
struct Pending {
    binding: ControlBinding,
    input: Option<SessionInput>,
    input_deadline: u64,
}
#[derive(Default)]
struct MailboxShard {
    pending: BTreeMap<Id, Pending>,
    reserved: std::collections::HashSet<Id>,
}
struct Mailboxes {
    prepared: std::sync::OnceLock<PreparedSessionControls>,
    shards: Vec<Mutex<MailboxShard>>,
    occupied: std::sync::atomic::AtomicUsize,
    limit: usize,
    count: std::sync::atomic::AtomicUsize,
}
/// Clone onto async/network tasks. Enqueue is synchronous, bounded and creates
/// no threads. Multiple samples for an object take a single pending slot.
#[derive(Resource, Clone)]
pub struct SessionIngress(Arc<Mailboxes>);
impl SessionIngress {
    pub fn capacity(&self) -> usize {
        self.0.limit
    }
    fn shard(&self, id: Id) -> &Mutex<MailboxShard> {
        let n = u128::from_be_bytes(id.0);
        &self.0.shards[(n ^ (n >> 64)) as usize % self.0.shards.len()]
    }
    fn release(&self, id: Id) {
        let mut shard = self.shard(id).lock().unwrap_or_else(|e| e.into_inner());
        if !shard.pending.contains_key(&id) && shard.reserved.remove(&id) {
            self.0
                .occupied
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn put(&self, new: Pending) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(prepared) = self.0.prepared.get() {
            if !prepared.accepts(new.binding)? {
                return Err(Error::Denied);
            }
        }
        let id = new.binding.object;
        let mut shard = self.shard(id).lock().map_err(|_| Error::Fenced)?;
        if let Some(old) = shard.pending.get_mut(&id) {
            if new.binding.epoch < old.binding.epoch
                || (new.binding.epoch == old.binding.epoch && new.binding != old.binding)
            {
                return Err(Error::Fenced);
            }
            if new.binding != old.binding
                || new.input.map(|i| i.sequence) > old.input.map(|i| i.sequence)
            {
                *old = new;
            }
        } else {
            if !shard.reserved.contains(&id) {
                self.0
                    .occupied
                    .fetch_update(Relaxed, Relaxed, |n| (n < self.0.limit).then_some(n + 1))
                    .map_err(|_| Error::Busy("input mailbox capacity"))?;
                shard.reserved.insert(id);
            }
            self.0.count.fetch_add(1, Relaxed);
            shard.pending.insert(id, new);
        }
        Ok(())
    }
    pub fn attach(&self, binding: ControlBinding) -> Result<()> {
        self.put(Pending {
            binding,
            input: None,
            input_deadline: 0,
        })
    }
    pub fn submit(&self, input: SessionInput) -> Result<()> {
        self.submit_until(input, u64::MAX)
    }
    pub fn submit_until(&self, input: SessionInput, expires_ms: u64) -> Result<()> {
        let now = control_now()?;
        let input_deadline = expires_ms.min(input.at_ms.saturating_add(500));
        if input.sequence == 0 || input.at_ms > now.saturating_add(100) || now >= input_deadline {
            return Err(invalid("session input outside time window"));
        }
        self.put(Pending {
            binding: input.binding,
            input: Some(input),
            input_deadline,
        })
    }
    pub fn pending(&self) -> usize {
        self.0.count.load(std::sync::atomic::Ordering::Relaxed)
    }
}
#[derive(Resource, Default)]
struct Controlled(BTreeMap<Id, (Pending, u64)>);
/// Install after MeshPlugin. The persistent host uses its existing authority store.
pub struct MeshSessionPlugin {
    pub capacity: std::num::NonZeroUsize,
}
impl Default for MeshSessionPlugin {
    fn default() -> Self {
        Self {
            capacity: std::num::NonZeroUsize::new(4096).unwrap(),
        }
    }
}
impl bevy_app::Plugin for MeshSessionPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        use bevy_ecs::schedule::{
            common_conditions::resource_exists, IntoSystemConfigs, IntoSystemSetConfigs,
        };
        let directory = app
            .world()
            .get_resource::<Store>()
            .cloned()
            .map(SessionDirectory::persistent)
            .unwrap_or_else(|| SessionDirectory::transient(self.capacity));
        app.mesh_component::<InputCursor>(
            0x5a5a_0001,
            1,
            Rules {
                visibility: Visibility::Local,
                persist: true,
                migrate: true,
            },
            Default::default(),
        )
        .expect("reserved mesh input cursor schema");
        app.insert_resource(directory)
            .init_resource::<Controlled>()
            .insert_resource(SessionIngress(Arc::new(Mailboxes {
                prepared: Default::default(),
                shards: (0..self.capacity.get().min(32))
                    .map(|_| Mutex::default())
                    .collect(),
                count: Default::default(),
                occupied: Default::default(),
                limit: self.capacity.get(),
            })))
            .add_systems(
                bevy_app::PreUpdate,
                apply_control
                    .in_set(MeshSystems::Control)
                    .after(MeshSystems::Authority),
            )
            .configure_sets(
                bevy_app::Update,
                (MeshSystems::Control, MeshSystems::GameplayInput).chain(),
            )
            .configure_sets(
                bevy_app::FixedUpdate,
                (MeshSystems::Control, MeshSystems::GameplayInput).chain(),
            )
            .add_systems(
                bevy_app::FixedFirst,
                apply_control.run_if(resource_exists::<PreparedSessionControls>),
            )
            .add_systems(
                bevy_app::FixedUpdate,
                apply_control
                    .in_set(MeshSystems::Control)
                    .run_if(resource_exists::<PreparedSessionControls>),
            )
            .add_systems(
                bevy_app::Update,
                apply_control
                    .in_set(MeshSystems::Control)
                    .run_if(resource_exists::<PreparedSessionControls>),
            );
    }
}
fn worker_states(
    world: &World,
    worker: Worker,
    objects: &[(Id, Id, u64)],
) -> Result<Vec<Option<ControlState>>> {
    if let Some(prepared) = world.get_resource::<PreparedSessionControls>() {
        return prepared.states(worker, objects);
    }
    world
        .get_resource::<SessionDirectory>()
        .ok_or(Error::Denied)?
        .for_worker(worker, objects)
}

/// Run on the simulation thread before acknowledging a new control barrier.
pub(crate) fn install_control_fence(world: &mut World, binding: ControlBinding) -> Result<()> {
    let Some(prepared) = world.get_resource::<PreparedSessionControls>() else {
        return Ok(());
    };
    let state = world.resource::<MeshState>();
    let entity = state.entities.get(&binding.object).copied();
    if prepared.accepts(binding)? {
        let e = entity.ok_or(Error::Fenced)?;
        let group = world.get::<Managed>(e).ok_or(Error::Fenced)?.group;
        let owner = state.authority(group).ok_or(Error::Fenced)?;
        if world.get::<Simulated>(e).is_none()
            || prepared.states(owner.worker, &[(binding.object, group, owner.generation)])?[0]
                .is_none_or(|s| s.binding != binding)
        {
            return Err(Error::Fenced);
        }
        world.resource::<SessionIngress>().attach(binding)?;
    } else if let Some(e) = entity {
        if world
            .get::<ControlBinding>(e)
            .is_some_and(|local| local.epoch <= binding.epoch)
        {
            world
                .entity_mut(e)
                .remove::<(ControlBinding, ValidatedInput, MeshReady)>();
        }
    }
    Ok(())
}
fn apply_control(world: &mut World) {
    if world.resource::<SessionIngress>().pending() == 0
        && world.resource::<Controlled>().0.is_empty()
    {
        return;
    }
    let start = std::time::Instant::now();
    let mut controlled = world.remove_resource::<Controlled>().unwrap();
    let ingress = world.resource::<SessionIngress>().clone();
    if ingress.pending() > 0 {
        for shard in &ingress.0.shards {
            let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            let items = std::mem::take(&mut shard.pending);
            ingress
                .0
                .count
                .fetch_sub(items.len(), std::sync::atomic::Ordering::Relaxed);
            drop(shard);
            for (id, pending) in items {
                if let Some((old, through)) = controlled.0.get_mut(&id) {
                    if pending.binding.epoch > old.binding.epoch {
                        *old = pending;
                        *through = 0;
                    } else if pending.binding == old.binding && pending.input.is_some() {
                        *old = pending;
                    }
                } else {
                    controlled.0.insert(id, (pending, 0));
                }
            }
        }
    }
    if !controlled.0.is_empty() {
        let now = control_now().unwrap_or(u64::MAX);
        let mut groups = BTreeMap::<Worker, Vec<_>>::new();
        let state = world.resource::<MeshState>();
        for &id in controlled.0.keys() {
            if let Some(&e) = state.entities.get(&id) {
                if let Some((m, a)) = world
                    .get::<Managed>(e)
                    .and_then(|m| Some((m, state.authority(m.group)?)))
                {
                    groups
                        .entry(a.worker)
                        .or_default()
                        .push((id, m.group, a.generation));
                }
            }
        }
        let mut next = BTreeMap::new();
        for (worker, ids) in groups {
            for batch in ids.chunks(MAX_TARGETS) {
                let states = worker_states(world, worker, batch);
                for (idx, &(id, _, _)) in batch.iter().enumerate() {
                    let (mut p, mut through) = controlled.0.remove(&id).unwrap();
                    let e = world.resource::<MeshState>().entities[&id];
                    if matches!(states, Err(Error::Busy(_))) {
                        // Fail closed for this schedule but keep the reservation
                        // and pending sample for the next bounded refresh.
                        world
                            .entity_mut(e)
                            .remove::<(ControlBinding, ValidatedInput, MeshReady)>();
                        next.insert(id, (p, through));
                        continue;
                    }
                    let current =
                        states.as_ref().ok().and_then(|s| s[idx]).filter(|s| {
                            s.binding == p.binding && world.get::<Simulated>(e).is_some()
                        });
                    if current.is_some() {
                        if world.get::<ControlBinding>(e) != Some(&p.binding) {
                            if let Some(c) = world
                                .get::<InputCursor>(e)
                                .filter(|c| c.binding == Some(p.binding))
                            {
                                through = through.max(c.through);
                            }
                            world
                                .entity_mut(e)
                                .remove::<ValidatedInput>()
                                .insert((p.binding, MeshReady));
                        }
                        if let Some(i) = p
                            .input
                            .take()
                            .filter(|i| i.sequence > through && now < p.input_deadline)
                        {
                            through = i.sequence;
                            let prepared = world.contains_resource::<PreparedSessionControls>();
                            let mut entity = world.entity_mut(e);
                            entity.insert(ValidatedInput {
                                sequence: i.sequence,
                                axes: i.axes,
                                at_ms: i.at_ms,
                                binding: p.binding,
                                expires_ms: p.input_deadline.min(i.at_ms.saturating_add(200)),
                            });
                            if prepared {
                                entity.entry::<InputCursor>().or_default();
                            } else {
                                entity.insert(InputCursor {
                                    binding: Some(p.binding),
                                    through,
                                });
                            }
                        }
                        if world.get::<ValidatedInput>(e).is_some_and(|i| {
                            now >= p.input_deadline || now.saturating_sub(i.at_ms) >= 200
                        }) {
                            world.entity_mut(e).remove::<ValidatedInput>();
                        }
                        next.insert(id, (p, through));
                    } else {
                        world
                            .entity_mut(e)
                            .remove::<(ControlBinding, ValidatedInput, MeshReady)>();
                        ingress.release(id);
                    }
                }
            }
        }
        for id in controlled.0.keys() {
            ingress.release(*id);
            if let Some(&entity) = world.resource::<MeshState>().entities.get(id) {
                world
                    .entity_mut(entity)
                    .remove::<(ControlBinding, ValidatedInput, MeshReady)>();
            }
        }
        controlled.0 = next;
    }
    world.insert_resource(controlled);
    if let Some(mut metrics) = world.get_resource_mut::<WorkerTimings>() {
        metrics.input_us += start.elapsed().as_micros() as u64;
    }
}
