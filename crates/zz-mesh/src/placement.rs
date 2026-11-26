use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Capacity {
    pub worker: Worker,
    pub tick_us: u64,
    pub budget_us: u64,
    pub used: u64,
    pub memory: u64,
    pub draining: bool,
}
#[derive(Clone, Debug)]
pub struct GroupLoad {
    pub group: Id,
    pub owner: Worker,
    pub cost_us: u64,
    pub bytes: u64,
    pub moved_at: u64,
    /// Estimated communication cost saved each tick if colocated with this peer.
    pub affinity: BTreeMap<Worker, u64>,
    pub cached: BTreeMap<Worker, u64>,
}
/// On-demand metrics for a provisioning controller. Group hints are game-supplied
/// microseconds. Affinity is derived from the worker's actual directed subscriptions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerLoad {
    pub worker: Worker,
    pub tick_us: u64,
    pub budget_us: u64,
    pub memory_bytes: u64,
    pub cache_bytes: u64,
    pub groups: usize,
    pub group_us: Option<u64>,
    pub affinity: BTreeMap<Worker, u64>,
    pub cached_required: u64,
    pub process: ProcessLoad,
}
/// CPU permille is measured across all threads (1000 is one busy core).
#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProcessLoad {
    pub resident_bytes: u64,
    pub cpu_permille: u64,
    pub simulation_and_peer_us: u64,
    pub request_work_us: u64,
    pub queue_wait_us: u64,
    pub pending_requests: usize,
    pub deadline_misses: u64,
    pub ecs_us: u64,
    pub peer_us: u64,
    pub checkpoint_us: u64,
    pub input_us: u64,
    pub action_us: u64,
    pub migration_us: u64,
    pub max_queue_depth: usize,
}
#[derive(Default, bevy_ecs::prelude::Resource)]
pub struct WorkerTimings {
    pub checkpoint_us: u64,
    pub input_us: u64,
    pub action_us: u64,
    pub migration_us: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Stay,
    Move(Worker),
    Overloaded,
}
pub fn place(
    group: &GroupLoad,
    pool: &[Capacity],
    tick: u64,
    min_residence: u64,
    transfer_budget: u64,
) -> Placement {
    let Some(current) = pool.iter().find(|c| c.worker == group.owner) else {
        return Placement::Overloaded;
    };
    if !current.draining && current.tick_us <= current.budget_us && current.used <= current.memory {
        return Placement::Stay;
    }
    if !current.draining && tick.saturating_sub(group.moved_at) < min_residence {
        return Placement::Overloaded;
    }
    let score = |c: &Capacity| {
        let cost = group
            .bytes
            .saturating_sub(group.cached.get(&c.worker).copied().unwrap_or(0));
        let communication = group
            .affinity
            .values()
            .copied()
            .sum::<u64>()
            .saturating_sub(group.affinity.get(&c.worker).copied().unwrap_or(0));
        c.tick_us
            .saturating_add(group.cost_us)
            .saturating_add(communication)
            .saturating_add(cost / 65536)
    };
    let best = pool
        .iter()
        .filter(|c| {
            !c.draining
                && c.worker != group.owner
                && c.used.saturating_add(group.bytes) <= c.memory
                && group
                    .bytes
                    .saturating_sub(group.cached.get(&c.worker).copied().unwrap_or(0))
                    <= transfer_budget
                && c.tick_us.saturating_add(group.cost_us) <= c.budget_us
        })
        .min_by(|a, b| {
            let a_relative = u128::from(score(a)) * u128::from(b.budget_us);
            let b_relative = u128::from(score(b)) * u128::from(a.budget_us);
            a_relative
                .cmp(&b_relative)
                .then_with(|| a.worker.cmp(&b.worker))
        });
    match best {
        Some(c)
            if current.draining
                || current.used > current.memory
                || u128::from(score(c)) * u128::from(current.budget_us)
                    < u128::from(current.tick_us) * u128::from(c.budget_us) =>
        {
            Placement::Move(c.worker)
        }
        _ => Placement::Overloaded,
    }
}
/// One turn per logical session; discrete actions remain in insertion order.
pub struct FairQueue {
    sessions: BTreeMap<Id, VecDeque<Intent>>,
    order: VecDeque<Id>,
    count: usize,
    bytes: usize,
    max: usize,
    per: usize,
}
impl FairQueue {
    pub fn new(capacity: usize, per_session: usize) -> Self {
        Self {
            sessions: BTreeMap::new(),
            order: VecDeque::new(),
            count: 0,
            bytes: 0,
            max: capacity,
            per: per_session,
        }
    }
    pub fn push(&mut self, i: Intent) -> Result<()> {
        let id = i.key.session;
        if i.bytes.len() > MAX_FIELD
            || self.count >= self.max
            || self.bytes.saturating_add(i.bytes.len()) > MAX_FRAME
            || self.sessions.get(&id).map_or(0, |q| q.len()) >= self.per
        {
            return Err(Error::Busy("session action queue"));
        }
        let q = self.sessions.entry(id).or_default();
        if q.is_empty() {
            self.order.push_back(id)
        }
        self.count += 1;
        self.bytes += i.bytes.len();
        q.push_back(i);
        Ok(())
    }
    pub fn drain(&mut self, max: usize, budget: Duration, mut f: impl FnMut(Intent)) {
        let start = std::time::Instant::now();
        for _ in 0..max {
            if start.elapsed() >= budget {
                break;
            }
            let Some(id) = self.order.pop_front() else {
                break;
            };
            let q = self.sessions.get_mut(&id).unwrap();
            let intent = q.pop_front().unwrap();
            self.count -= 1;
            self.bytes -= intent.bytes.len();
            if q.is_empty() {
                self.sessions.remove(&id);
            } else {
                self.order.push_back(id)
            }
            f(intent);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn len(&self) -> usize {
        self.count
    }
}
