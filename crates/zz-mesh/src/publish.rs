use crate::*;
use bevy_ecs::prelude::World;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
type Edge = (Id, Recipient);
#[derive(Default)]
struct Sample {
    dirty: bool,
    due: u64,
    // None is unsampled. Some(None) is an observed absence and goes on the wire.
    value: Option<Option<Field>>,
}
#[derive(Default)]
struct Link {
    epoch: u64,
    serial: u64,
    acked: u64,
    generation: u64,
    fields: BTreeMap<u32, Sample>,
    deadline: Option<u64>,
    /// Revision of the latest unacknowledged sample. Retries send its full projection.
    pending_revision: Option<u64>,
}
/// Each dirty object touches only its current recipients. Due times are kept per field.
/// The transport queues one replaceable frame per edge, with an acknowledged base.
pub struct Publisher {
    links: HashMap<Edge, Link>,
    work: BTreeMap<u64, BTreeSet<Edge>>,
    queued: usize,
    limit: usize,
    pub encodes: u64,
    pub peak: usize,
}
impl Publisher {
    pub fn new(limit: usize) -> Self {
        Self {
            links: HashMap::new(),
            work: BTreeMap::new(),
            queued: 0,
            limit,
            encodes: 0,
            peak: 0,
        }
    }
    fn schedule(&mut self, k: Edge, t: u64) {
        let Some(link) = self.links.get_mut(&k) else {
            return;
        };
        if let Some(old) = link.deadline {
            if old <= t {
                return;
            }
            if let Some(set) = self.work.get_mut(&old) {
                set.remove(&k);
                if set.is_empty() {
                    self.work.remove(&old);
                }
            }
        } else {
            self.queued += 1;
        }
        link.deadline = Some(t);
        self.work.entry(t).or_default().insert(k);
        self.peak = self.peak.max(self.queued);
    }
    pub fn route(&mut self, c: &InterestChange) -> Result<()> {
        let k = (c.object, c.to);
        if let Some(subscription) = &c.subscription {
            if !self.links.contains_key(&k) && self.links.len() >= self.limit {
                return Err(Error::Busy("replication links"));
            }
            let deadline = self.links.get(&k).and_then(|link| link.deadline);
            self.links.insert(
                k,
                Link {
                    epoch: c.epoch,
                    deadline,
                    fields: subscription
                        .components
                        .keys()
                        .map(|&id| {
                            (
                                id,
                                Sample {
                                    dirty: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                    ..Default::default()
                },
            );
            self.schedule(k, 0);
        } else {
            if let Some(t) = self.links.remove(&k).and_then(|l| l.deadline) {
                self.queued -= 1;
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
            if let Some(l) = self.links.get_mut(&(id, to)) {
                for &schema in sub.components.keys() {
                    l.fields.entry(schema).or_default().dirty = true;
                }
            }
            self.schedule((id, to), tick);
        }
    }
    pub fn ack(&mut self, to: Recipient, id: Id, epoch: u64, serial: u64) {
        let Some(l) = self.links.get_mut(&(id, to)) else {
            return;
        };
        if l.epoch != epoch {
            return;
        }
        if l.pending_revision.is_some() && l.serial == serial {
            l.pending_revision = None;
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
            self.queued -= set.len();
            for key in &set {
                if let Some(link) = self.links.get_mut(key) {
                    link.deadline = None;
                }
            }
            ready.append(&mut set);
        }
        let mut batches: BTreeMap<Recipient, Vec<Update>> = BTreeMap::new();
        let mut cached_object = None;
        let mut peer_fields = Vec::new();
        let mut ready = ready.into_iter();
        while let Some((id, to)) = ready.next() {
            let Some(l) = self.links.get_mut(&(id, to)) else {
                continue;
            };
            let generation = gen(id);
            let Some(p) = d.presence(id) else { continue };
            let Some(edges) = d.edges(id) else { continue };
            let Some(sub) = edges.get(&to) else {
                continue;
            };
            let share = edges.len() > 1 && matches!(to, Recipient::Worker(_));
            if share && cached_object != Some(id) {
                peer_fields.clear();
                cached_object = Some(id);
            }
            let mut next = None;
            let revision = w.resource::<MeshState>().version(id);
            if l.generation != generation {
                l.acked = 0;
                for sample in l.fields.values_mut() {
                    *sample = Sample {
                        dirty: true,
                        ..Default::default()
                    };
                }
                l.pending_revision = None;
            }
            // After ACK, sampled is precisely the acknowledged base. With an
            // outstanding serial the receiver may have lost its delta: send full.
            let full = l.acked == 0 || l.pending_revision.is_some();
            let mut fields = BTreeMap::new();
            let mut sampled = false;
            let projection = (|| -> Result<()> {
                for (&schema, dem) in &sub.components {
                    let Some(sample) = l.fields.get_mut(&schema) else {
                        continue;
                    };
                    if !sample.dirty {
                        continue;
                    }
                    let deadline = sample.due;
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
                    // All worker recipients of this object see the same worker view
                    // at a given detail. Client projections still evaluate their principal.
                    let value = if share {
                        let key = (schema, dem.detail);
                        let i = match peer_fields.binary_search_by_key(&key, |(k, _)| *k) {
                            Ok(i) => i,
                            Err(i) => {
                                let field = project_component(w, id, schema, view)?;
                                self.encodes += 1;
                                peer_fields.insert(i, (key, field));
                                i
                            }
                        };
                        std::borrow::Cow::Borrowed(&peer_fields[i].1)
                    } else {
                        self.encodes += 1;
                        std::borrow::Cow::Owned(project_component(w, id, schema, view)?)
                    };
                    if sample.value.as_ref() != Some(value.as_ref()) {
                        sampled = true;
                        if !full {
                            fields.insert(schema, value.as_ref().clone());
                        }
                        sample.value = Some(value.into_owned());
                    }
                    sample.dirty = false;
                    sample.due = tick + u64::from(dem.period.max(1));
                }
                Ok(())
            })();
            if let Err(error) = projection {
                // Some fields of this edge may already have been sampled. Retry
                // as a full frame. Later ready edges have not been visited yet.
                l.acked = 0;
                l.pending_revision = None;
                for sample in l.fields.values_mut() {
                    *sample = Sample {
                        dirty: true,
                        ..Default::default()
                    };
                }
                self.schedule((id, to), tick);
                for edge in ready {
                    self.schedule(edge, tick);
                }
                return Err(error);
            }
            if sampled {
                l.serial += 1;
                l.pending_revision = Some(revision);
            }
            if let Some(revision) = l.pending_revision {
                if full {
                    fields = l
                        .fields
                        .iter()
                        .filter_map(|(&key, s)| s.value.as_ref().map(|v| (key, v.clone())))
                        .collect();
                }
                batches.entry(to).or_default().push(Update {
                    object: id,
                    epoch: l.epoch,
                    generation,
                    revision,
                    serial: l.serial,
                    base: if full { None } else { Some(l.acked) },
                    fields,
                    deleted: false,
                });
            }
            l.generation = generation;
            if l.pending_revision.is_some() {
                let delay = tick + 4;
                next = Some(next.map_or(delay, |v| v.min(delay)));
            }
            if let Some(t) = next {
                self.schedule((id, to), t);
            }
        }
        Ok(batches)
    }
}
