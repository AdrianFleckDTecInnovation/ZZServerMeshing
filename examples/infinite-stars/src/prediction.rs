use super::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
/// Used by the example client on both the local and TLS paths.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Predicted {
    Mine(MineBlocks),
    Drop(DropItems),
    Pickup(Pickup),
    Build(Build),
    Trade(Trade),
}
impl Predicted {
    fn intent(&self, key: ActionKey, group: Id) -> mesh::Result<Intent> {
        match self {
            Self::Mine(a) => Intent::typed(key, group, a),
            Self::Drop(a) => Intent::typed(key, group, a),
            Self::Pickup(a) => Intent::typed(key, group, a),
            Self::Build(a) => Intent::typed(key, group, a),
            Self::Trade(a) => Intent::typed(key, group, a),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Visible {
    pub inventory: Inventory,
    pub drops: BTreeMap<Id, Stack>,
    pub blocks: BTreeMap<(Id, u16), bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prediction {
    pub session: Id,
    pub group: Id,
    pub revision: u64,
    pub decided: u64,
    next: u64,
    confirmed: Visible,
    pending: VecDeque<(Intent, Predicted)>,
    rejected: BTreeSet<u64>,
}
impl Prediction {
    pub fn new(session: Id, group: Id, inventory: Inventory) -> Self {
        Self {
            session,
            group,
            revision: 0,
            decided: 0,
            next: 1,
            confirmed: Visible {
                inventory,
                ..Default::default()
            },
            pending: VecDeque::new(),
            rejected: BTreeSet::new(),
        }
    }
    pub fn submit(&mut self, action: Predicted, after: Option<u64>) -> mesh::Result<Intent> {
        if self.pending.len() >= 32 {
            return Err(mesh::Error::Busy("client predictions"));
        }
        let mut i = action.intent(
            ActionKey {
                session: self.session,
                seq: self.next,
            },
            self.group,
        )?;
        if after.is_some_and(|n| n >= self.next) {
            return Err(invalid("invalid prediction dependency"));
        }
        i.after = after;
        self.next += 1;
        self.pending.push_back((i.clone(), action));
        Ok(i)
    }
    pub fn retry(&self) -> Vec<Intent> {
        self.pending.iter().map(|(i, _)| i.clone()).collect()
    }
    pub fn receive(&mut self, r: &Receipt) -> mesh::Result<()> {
        if r.key.session != self.session {
            return Err(invalid("wrong client session"));
        }
        if r.key.seq <= self.decided {
            return Ok(());
        }
        if self.pending.front().map(|(i, _)| i.key) != Some(r.key) || r.revision < self.revision {
            return Err(invalid("missing action result or stale revision"));
        }
        match &r.outcome {
            Outcome::Rejected(_) => {
                self.rejected.insert(r.key.seq);
            }
            Outcome::Committed(bytes) => {
                let effects: Effects = decode(bytes)?;
                self.confirmed.inventory = effects.inventory;
                self.confirmed.drops.extend(effects.drops);
                for id in effects.picked {
                    self.confirmed.drops.remove(&id);
                }
                for (t, solid) in effects.blocks {
                    self.confirmed.blocks.insert((t.chunk, t.index), solid);
                }
            }
        }
        self.pending.pop_front();
        self.decided = r.key.seq;
        self.revision = r.revision;
        self.rejected.retain(|seq| {
            self.pending.iter().any(|(i, _)| i.after == Some(*seq))
                || *seq > self.decided.saturating_sub(RECEIPTS)
        });
        Ok(())
    }
    pub fn resync(&mut self, revision: u64, through: u64, world: Visible) -> mesh::Result<()> {
        if through < self.decided || revision < self.revision {
            return Err(invalid("old resynchronization"));
        }
        self.pending.retain(|(intent, _)| intent.key.seq > through);
        self.confirmed = world;
        self.decided = through;
        self.revision = revision;
        self.next = self.next.max(through.saturating_add(1));
        self.rejected.clear();
        Ok(())
    }
    pub fn visible(&self) -> Visible {
        let mut v = self.confirmed.clone();
        let mut rejected = self.rejected.clone();
        for (i, a) in &self.pending {
            if i.after.is_some_and(|d| rejected.contains(&d)) {
                rejected.insert(i.key.seq);
                continue;
            }
            let mut next = v.clone();
            let result = (|| -> mesh::Result<()> {
                match a {
                    Predicted::Drop(d) => {
                        for (n, s) in d.0.iter().enumerate() {
                            next.inventory.take(s.kind, s.count)?;
                            next.drops.insert(drop_id(i.key, n), s.clone());
                        }
                    }
                    Predicted::Mine(m) => {
                        for t in &m.0 {
                            if next.blocks.insert((t.chunk, t.index), false) == Some(false) {
                                return Err(invalid("predicted empty block"));
                            }
                            next.inventory.add(STONE, 1)?;
                        }
                    }
                    Predicted::Build(b) => {
                        for t in &b.0 {
                            next.inventory.take(STONE, 1)?;
                            next.blocks.insert((t.chunk, t.index), true);
                        }
                    }
                    Predicted::Pickup(p) => {
                        for id in &p.0 {
                            if let Some(s) = next.drops.remove(id) {
                                next.inventory.add(s.kind, s.count)?;
                            }
                        }
                    }
                    Predicted::Trade(_) => {} // The counterparty's private state is confirmed by the server.
                }
                Ok(())
            })();
            if result.is_ok() {
                v = next
            } else {
                rejected.insert(i.key.seq);
            }
        }
        v
    }
}
