//! Periodic snapshots may write in the background. Only a confirmed commit advances
//! the ECS baseline. Explicit action and handoff barriers retain their synchronous contract.
use super::*;
use std::{
    num::NonZeroUsize,
    sync::{
        mpsc::{Receiver, TryRecvError},
        Mutex,
    },
};

struct InFlight {
    authority: Authority,
    base: u64,
    records: Vec<Record>,
    bytes: usize,
    answer: Mutex<Receiver<Result<u64>>>,
}

/// Opt in with `app.insert_resource(AsyncCheckpoints::new(...))` after registering the worker.
/// Clones of one executor may be shared between worlds. Each group has at most one pending commit.
#[derive(Resource)]
pub struct AsyncCheckpoints {
    executor: BulkExecutor,
    slots: usize,
    capacity_bytes: usize,
    in_flight: BTreeMap<Id, InFlight>,
    bytes: usize,
}
impl AsyncCheckpoints {
    pub fn new(executor: BulkExecutor, groups: NonZeroUsize, bytes: NonZeroUsize) -> Self {
        Self {
            executor,
            slots: groups.get(),
            capacity_bytes: bytes.get(),
            in_flight: BTreeMap::new(),
            bytes: 0,
        }
    }
    pub fn pending(&self) -> (usize, usize) {
        (self.in_flight.len(), self.bytes)
    }
    pub(super) fn executor(&self) -> BulkExecutor {
        self.executor.clone()
    }
}
pub(super) fn pending(w: &World, g: Id) -> bool {
    w.get_resource::<AsyncCheckpoints>()
        .is_some_and(|q| q.in_flight.contains_key(&g))
}
pub(super) fn retry(w: &mut World, g: Id, records: &[Record]) {
    let mut st = w.resource_mut::<MeshState>();
    for r in records {
        if r.deleted {
            st.removed.insert(r.id, g);
        } else {
            st.pending.urgent.insert(r.id);
        }
    }
}
pub(super) fn acknowledge(w: &mut World, g: Id, a: Authority, rev: u64, records: &mut [Record]) {
    let mut st = w.resource_mut::<MeshState>();
    for r in records {
        if r.deleted {
            st.saved.remove(&r.id);
            st.versions.remove(&r.id);
        } else {
            st.versions.insert(r.id, rev);
            st.saved.insert(r.id, std::mem::take(&mut r.fields));
        }
    }
    st.groups.insert(g, (a, rev));
}
pub(super) fn schedule(
    w: &mut World,
    db: &Store,
    g: Id,
    a: Authority,
    base: u64,
    records: &mut Vec<Record>,
) -> Result<()> {
    let queue = w.resource::<AsyncCheckpoints>();
    if queue.in_flight.contains_key(&g) || queue.in_flight.len() >= queue.slots {
        return Err(Error::Busy("checkpoint queue"));
    }
    let claims = checkpoint_claims(w, records)?;
    let claim_bytes = claims
        .values()
        .flatten()
        .map(|m| 128 + m.dependencies.len() * 48)
        .sum::<usize>();
    let weight = |r: &Record| {
        192 + r
            .fields
            .values()
            .map(|f| 128 + f.bytes.len())
            .sum::<usize>()
            * 2
    };
    let available = queue.capacity_bytes.saturating_sub(queue.bytes);
    let mut bytes = claim_bytes;
    let count = records
        .iter()
        .take_while(|r| {
            let next = bytes.saturating_add(weight(r));
            if next > available {
                false
            } else {
                bytes = next;
                true
            }
        })
        .count();
    if count == 0 {
        return Err(Error::Busy(if queue.bytes == 0 {
            "single checkpoint exceeds configured byte budget"
        } else {
            "checkpoint queue"
        }));
    }
    if count < records.len() {
        let deferred = records.split_off(count);
        retry(w, g, &deferred);
    }
    let submitted = records.clone();
    let claims = claims
        .into_iter()
        .filter(|(id, _)| submitted.iter().any(|r| r.id == *id))
        .collect();
    let db = db.clone();
    let mut queue = w.resource_mut::<AsyncCheckpoints>();
    let answer = queue
        .executor
        .submit(move || db.checkpoint_data(g, a, base, submitted, &claims))?;
    queue.bytes += bytes;
    queue.in_flight.insert(
        g,
        InFlight {
            authority: a,
            base,
            records: std::mem::take(records),
            bytes,
            answer: Mutex::new(answer),
        },
    );
    Ok(())
}
fn finish(w: &mut World, g: Id, wait: bool) -> Result<()> {
    let mut queue = w.resource_mut::<AsyncCheckpoints>();
    let Some(job) = queue.in_flight.get_mut(&g) else {
        return Ok(());
    };
    let answer = job
        .answer
        .get_mut()
        .map_err(|_| invalid("checkpoint receiver poisoned"))?;
    let outcome = if wait {
        answer
            .recv()
            .unwrap_or_else(|_| Err(invalid("checkpoint worker stopped")))
    } else {
        match answer.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => Err(invalid("checkpoint worker stopped")),
        }
    };
    let mut job = queue.in_flight.remove(&g).unwrap();
    queue.bytes -= job.bytes;
    if w.resource::<MeshState>().group_state(g) != Some((job.authority, job.base)) {
        return Err(Error::Fenced);
    }
    match outcome {
        Ok(rev) => {
            acknowledge(w, g, job.authority, rev, &mut job.records);
            Ok(())
        }
        Err(e) => {
            retry(w, g, &job.records);
            Err(e)
        }
    }
}
pub(super) fn barrier(w: &mut World, g: Id) -> Result<()> {
    if pending(w, g) {
        finish(w, g, true)
    } else {
        Ok(())
    }
}
pub(super) fn collect(w: &mut World) {
    let Some(queue) = w.get_resource::<AsyncCheckpoints>() else {
        return;
    };
    let groups: Vec<_> = queue.in_flight.keys().copied().collect();
    for g in groups {
        if let Err(e) = finish(w, g, false) {
            w.resource_mut::<MeshState>().report(e.to_string());
        }
    }
}
