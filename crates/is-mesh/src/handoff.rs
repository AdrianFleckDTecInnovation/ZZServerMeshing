use crate::authority_store::Operation as Op;
use crate::store::{authority, fence, live, snapshot};
use crate::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Preparing,
    Frozen,
    Ready,
    Switched,
    Aborted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub id: Id,
    pub group: Id,
    pub source: Authority,
    pub target: Worker,
}
fn status(c: &Connection, t: Handoff) -> Result<Phase> {
    let row: Option<(Id, Vec<u8>, Vec<u8>, u8)> = c
        .query_row(
            "SELECT unit,source,target,phase FROM handoffs WHERE ticket=?",
            [t.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((g, s, d, p)) = row else {
        return Err(Error::Missing(format!("handoff {}", t.id)));
    };
    if g != t.group || decode::<Authority>(&s)? != t.source || decode::<Worker>(&d)? != t.target {
        return Err(Error::Fenced);
    }
    match p {
        0 => Ok(Phase::Preparing),
        1 => Ok(Phase::Frozen),
        2 => Ok(Phase::Ready),
        3 => Ok(Phase::Switched),
        4 => Ok(Phase::Aborted),
        _ => Err(invalid("invalid durable transfer phase")),
    }
}
fn frozen_snapshot(c: &Connection, t: Handoff) -> Result<Snapshot> {
    let mut s = snapshot(c, t.group)?;
    let mut q = c.prepare("SELECT body FROM handoff_records WHERE ticket=? ORDER BY id")?;
    let mut records = vec![];
    for b in q.query_map([t.id], |r| r.get::<_, Vec<u8>>(0))? {
        records.push(decode(&b?)?);
    }
    s.records = records;
    Ok(s)
}
impl Store {
    pub fn begin_handoff(&self, t: Handoff) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Begin(t));
        }
        self.write(|c|{if let Ok(p)=status(c,t){return if p==Phase::Aborted{Err(Error::Fenced)}else{Ok(())}}
            if t.source.worker==t.target{return Err(invalid("handoff to same worker"))}
            fence(c,t.group,t.source,true)?;live(c,t.target)?;crate::store::accepting(c,t.target)?;
            c.execute("DELETE FROM handoffs WHERE unit=? AND phase>=3 AND ticket NOT IN (SELECT ticket FROM handoffs WHERE unit=? ORDER BY rowid DESC LIMIT 15)",params![t.group,t.group])?;
            c.execute("INSERT INTO handoffs(ticket,unit,source,target,phase) VALUES(?,?,?,?,0)",params![t.id,t.group,encode(&t.source)?,encode(&t.target)?])?;Ok(())})
    }
    pub fn frozen(&self, t: Handoff) -> Result<Snapshot> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Frozen(t));
        }
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        if !matches!(status(&tx, t)?, Phase::Frozen | Phase::Ready) {
            return Err(Error::Fenced);
        }
        let s = frozen_snapshot(&tx, t)?;
        tx.commit()?;
        Ok(s)
    }
    pub fn handoff_phase(&self, t: Handoff) -> Result<Phase> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Phase(t));
        }
        status(&*self.lock()?, t)
    }
    /// Freeze only the base observed by the simulation. The caller retries preparation on a race.
    pub fn freeze(&self, t: Handoff, base: u64, canonical: &[Record]) -> Result<Snapshot> {
        self.freeze_checkpoint(t, base, canonical, canonical)
    }
    pub fn freeze_checkpoint(
        &self,
        t: Handoff,
        base: u64,
        canonical: &[Record],
        checkpoint: &[Record],
    ) -> Result<Snapshot> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Freeze(t, base, canonical.to_vec(), checkpoint.to_vec()));
        }
        self.write(|c| {
            match status(c, t)? {
                Phase::Frozen | Phase::Ready => return frozen_snapshot(c, t),
                Phase::Preparing => {}
                _ => return Err(Error::Fenced),
            }
            if fence(c, t.group, t.source, true)? != base {
                return Err(Error::Fenced);
            }
            let mut s = snapshot(c, t.group)?;
            if canonical.len() != s.records.len() {
                return Err(invalid("group membership changed during preparation"));
            }
            let mut ordered = canonical.to_vec();
            ordered.sort_by_key(|r| r.id);
            for (stored, exact) in s.records.iter().zip(&ordered) {
                if (stored.id, stored.group, stored.revision, stored.deleted)
                    != (exact.id, exact.group, exact.revision, exact.deleted)
                {
                    return Err(Error::Fenced);
                }
                c.execute(
                    "INSERT INTO handoff_records VALUES(?,?,?)",
                    params![t.id, exact.id, encode(exact)?],
                )?;
            }
            if checkpoint.len() != ordered.len() {
                return Err(Error::Fenced);
            }
            for (exact, ck) in ordered.iter().zip(checkpoint) {
                if (exact.id, exact.group, exact.revision, exact.deleted)
                    != (ck.id, ck.group, ck.revision, ck.deleted)
                {
                    return Err(Error::Fenced);
                }
                c.execute(
                    "INSERT INTO handoff_checkpoints VALUES(?,?,?)",
                    params![t.id, ck.id, crate::store::checkpoint_body(ck)?],
                )?;
            }
            s.records = ordered;
            let hash = s.hash()?;
            c.execute(
                "UPDATE units SET frozen=? WHERE id=?",
                params![t.id, t.group],
            )?;
            c.execute(
                "UPDATE handoffs SET phase=1,rev=?,manifest=? WHERE ticket=?",
                params![base, hash.0.as_slice(), t.id],
            )?;
            Ok(s)
        })
    }
    pub fn acknowledge(&self, t: Handoff, target: Worker, staged: &Prepared) -> Result<()> {
        self.acknowledge_hash(t, target, staged.hash())
    }
    pub(crate) fn acknowledge_hash(&self, t: Handoff, target: Worker, hash: Digest) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Ready(t, target, hash));
        }
        self.write(|c| {
            if target != t.target {
                return Err(Error::Denied);
            }
            live(c, target)?;
            if !matches!(status(c, t)?, Phase::Frozen | Phase::Ready) {
                return Err(Error::Fenced);
            }
            let matched = c.execute(
                "UPDATE handoffs SET phase=2 WHERE ticket=? AND manifest=?",
                params![t.id, hash.0.as_slice()],
            )?;
            if matched == 0 {
                return Err(Error::Fenced);
            }
            Ok(())
        })
    }
    pub fn switch(&self, t: Handoff) -> Result<Authority> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Switch(t));
        }
        self.write(|c|{match status(c,t)?{
            Phase::Switched=>return Ok(Authority{worker:t.target,generation:t.source.generation+1}),Phase::Ready=>{},_=>return Err(Error::Fenced)}
            fence(c,t.group,t.source,false)?;live(c,t.target)?;
            let n=c.execute("UPDATE units SET slot=?,inc=?,gen=gen+1,frozen=NULL WHERE id=? AND frozen=?",
                params![t.target.slot,t.target.incarnation,t.group,t.id])?;
            if n!=1{return Err(Error::Fenced)}
            c.execute("UPDATE documents SET body=h.body FROM handoff_checkpoints h WHERE h.ticket=? AND h.id=documents.id",[t.id])?;
            c.execute("UPDATE handoffs SET phase=3 WHERE ticket=?",[t.id])?;authority(c,t.group)})
    }
    /// Resolves an ambiguous acknowledgement against the same serialized durable decision.
    pub fn abort_handoff(&self, t: Handoff) -> Result<Phase> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Abort(t));
        }
        self.write(|c| {
            match status(c, t)? {
                Phase::Switched => return Ok(Phase::Switched),
                Phase::Aborted => return Ok(Phase::Aborted),
                _ => {}
            }
            fence(c, t.group, t.source, false)?;
            c.execute(
                "UPDATE units SET frozen=NULL WHERE id=? AND frozen=?",
                params![t.group, t.id],
            )?;
            c.execute("UPDATE handoffs SET phase=4 WHERE ticket=?", [t.id])?;
            Ok(Phase::Aborted)
        })
    }
}
