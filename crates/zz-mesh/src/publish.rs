use crate::*;
use bevy_ecs::prelude::World;
use std::collections::{BTreeMap, BTreeSet};

pub trait Routing {
    fn presence(&self, id: Id) -> Option<Presence>;
    fn edges(&self, id: Id) -> Option<&BTreeMap<Recipient, Subscription>>;
}
impl Routing for Directory {
    fn presence(&self, id: Id) -> Option<Presence> {
        self.presence(id)
    }
    fn edges(&self, id: Id) -> Option<&BTreeMap<Recipient, Subscription>> {
        self.edges(id)
    }
}
type Edge = (Recipient, Id);
#[derive(Default)]
struct Link {
    epoch: u64,
    serial: u64,
    acked: u64,
    revision: u64,
    generation: u64,
    last: BTreeMap<u32, Option<Field>>,
    sampled: BTreeMap<u32, Option<Field>>,
    due: BTreeMap<u32, u64>,
    dirty: BTreeSet<u32>,
    pending: Option<Update>,
}
/// Each dirty object touches only its current recipients. Due times are kept per field.
/// The transport queues one replaceable frame per edge, with an acknowledged base.
pub struct Publisher {
    links: BTreeMap<Edge, Link>,
    work: BTreeMap<u64, BTreeSet<Edge>>,
    scheduled: BTreeMap<Edge, u64>,
    limit: usize,
    pub encodes: u64,
    pub peak: usize,
}
impl Publisher {
    pub fn new(limit: usize) -> Self {
        Self {
            links: BTreeMap::new(),
            work: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            limit,
            encodes: 0,
            peak: 0,
        }
    }
    fn schedule(&mut self, k: Edge, t: u64) {
        if let Some(old) = self.scheduled.get(&k).copied() {
            if old <= t {
                return;
            }
            if let Some(set) = self.work.get_mut(&old) {
                set.remove(&k);
                if set.is_empty() {
                    self.work.remove(&old);
                }
            }
        }
        self.scheduled.insert(k, t);
        self.work.entry(t).or_default().insert(k);
        self.peak = self.peak.max(self.scheduled.len());
    }
    pub fn route(&mut self, c: &InterestChange) -> Result<()> {
        let k = (c.to, c.object);
        if c.subscription.is_some() {
            if !self.links.contains_key(&k) && self.links.len() >= self.limit {
                return Err(Error::Busy("replication links"));
            }
            self.links.insert(
                k,
                Link {
                    epoch: c.epoch,
                    dirty: c
                        .subscription
                        .as_ref()
                        .unwrap()
                        .components
                        .keys()
                        .copied()
                        .collect(),
                    ..Default::default()
                },
            );
            self.schedule(k, 0);
        } else {
            self.links.remove(&k);
            if let Some(t) = self.scheduled.remove(&k) {
                if let Some(set) = self.work.get_mut(&t) {
                    set.remove(&k);
                    if set.is_empty() {
                        self.work.remove(&t);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn changed(&mut self, d: &impl Routing, id: Id, tick: u64) {
        for (&to, sub) in d.edges(id).into_iter().flatten() {
            if let Some(l) = self.links.get_mut(&(to, id)) {
                l.dirty.extend(sub.components.keys());
            }
            self.schedule((to, id), tick);
        }
    }
    pub fn ack(&mut self, to: Recipient, id: Id, epoch: u64, serial: u64) {
        let Some(l) = self.links.get_mut(&(to, id)) else {
            return;
        };
        if l.epoch != epoch {
            return;
        }
        if l.pending.as_ref().is_some_and(|p| p.serial == serial) {
            let p = l.pending.take().unwrap();
            if p.base.is_none() {
                l.last.clear();
            }
            l.last.extend(p.fields);
            l.acked = serial;
        }
    }
    pub fn flush(
        &mut self,
        w: &World,
        d: &impl Routing,
        tick: u64,
        gen: impl Fn(Id) -> u64,
    ) -> Result<BTreeMap<Recipient, Vec<Update>>> {
        let mut ready = BTreeSet::new();
        while self.work.first_key_value().is_some_and(|(t, _)| *t <= tick) {
            let (_, mut set) = self.work.pop_first().unwrap();
            for k in &set {
                self.scheduled.remove(k);
            }
            ready.append(&mut set);
        }
        let mut batches: BTreeMap<Recipient, Vec<Update>> = BTreeMap::new();
        for (to, id) in ready {
            let generation = gen(id);
            let Some(p) = d.presence(id) else { continue };
            let Some(sub) = d.edges(id).and_then(|e| e.get(&to)) else {
                continue;
            };
            let Some(l) = self.links.get_mut(&(to, id)) else {
                continue;
            };
            let mut next = None;
            let revision = w.resource::<MeshState>().version(id);
            if l.generation != generation {
                l.last.clear();
                l.acked = 0;
                l.sampled.clear();
                l.due.clear();
                l.dirty.extend(sub.components.keys());
                l.pending = None;
            }
            let mut sampled = false;
            for (&schema, dem) in &sub.components {
                if !l.dirty.contains(&schema) {
                    continue;
                }
                let deadline = l.due.get(&schema).copied().unwrap_or(0);
                if tick < deadline {
                    next = Some(next.map_or(deadline, |v: u64| v.min(deadline)));
                    continue;
                }
                let view = match to {
                    Recipient::Worker(_) => View::Worker { detail: dem.detail },
                    Recipient::Client(principal) => View::Client {
                        principal,
                        owner: p.principal,
                        detail: dem.detail,
                    },
                };
                let val = project_component(w, id, schema, view)?;
                self.encodes += 1;
                sampled |= l.sampled.get(&schema) != Some(&val);
                l.sampled.insert(schema, val);
                l.dirty.remove(&schema);
                l.due.insert(schema, tick + u64::from(dem.period.max(1)));
            }
            let full = l.acked == 0 || l.pending.is_some();
            let fields: BTreeMap<_, _> = l
                .sampled
                .iter()
                .filter(|(k, v)| full || l.last.get(k) != Some(v))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            if sampled && !fields.is_empty() {
                l.serial += 1;
                let u = Update {
                    object: id,
                    epoch: l.epoch,
                    generation,
                    revision,
                    serial: l.serial,
                    base: if full { None } else { Some(l.acked) },
                    fields,
                    deleted: false,
                };
                l.pending = Some(u);
            }
            if !sampled {
                if let Some(u) = &mut l.pending {
                    if u.base.is_some() {
                        u.base = None;
                        u.fields = l.sampled.clone();
                    }
                }
            }
            if let Some(u) = &l.pending {
                batches.entry(to).or_default().push(u.clone());
            }
            l.generation = generation;
            l.revision = revision;
            if l.pending.is_some() {
                let delay = tick + 4;
                next = Some(next.map_or(delay, |v| v.min(delay)));
            }
            if let Some(t) = next {
                self.schedule((to, id), t);
            }
        }
        Ok(batches)
    }
}
