//! A narrow lease for the final local ECS swap. It is acquired *after* the
//! snapshot has been decoded, and never holds a SQLite transaction across a
//! network request or frame. Only mutations to the exact group or incarnation
//! conflict. An observer can reap an abandoned lease after one full monotonic
//! interval from its first sight of the committed random nonce. This requires
//! neither synchronized clocks nor a live issuing process.
use super::*;
use std::{sync::OnceLock, time::Instant};

pub(crate) const INSTALL_WINDOW: Duration = Duration::from_millis(160);
const RECOVERY_WINDOW: Duration = Duration::from_millis(200);
const MAX_OBSERVATIONS: usize = 4096;
pub(crate) const INSTALL_BUSY: &str = "group installation in progress";

pub(super) fn permit_write(c: &Connection, group: Id, token: Id) -> Result<()> {
    static OBSERVED: OnceLock<Mutex<BTreeMap<Id, Instant>>> = OnceLock::new();
    let now = Instant::now();
    let mut entries = OBSERVED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| Error::Fenced)?;
    if !entries.contains_key(&token) && entries.len() >= MAX_OBSERVATIONS {
        entries.retain(|_, seen| now.duration_since(*seen) < RECOVERY_WINDOW);
        if entries.len() >= MAX_OBSERVATIONS {
            return Err(Error::Busy(INSTALL_BUSY));
        }
    }
    if now.duration_since(*entries.entry(token).or_insert(now)) < RECOVERY_WINDOW {
        return Err(Error::Busy(INSTALL_BUSY));
    }
    drop(entries);
    // Start/recovery decisions and the compared write share a transaction. A
    // different grant (including a grant from a restarted worker) is untouched.
    c.execute(
        "DELETE FROM group_installations WHERE unit=? AND token=?",
        params![group, token],
    )?;
    Ok(())
}

pub(super) fn permit_restart(c: &Connection, slot: u32) -> Result<()> {
    let existing: Option<(Id, Id)> = c
        .prepare_cached("SELECT unit,token FROM group_installations WHERE slot=?")?
        .query_row([slot], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((group, token)) = existing {
        permit_write(c, group, token)?;
    }
    Ok(())
}

impl Store {
    /// Check identity and group in the same transaction that grants the first
    /// install. No document, published name or provisional ECS owner is created.
    pub(crate) fn reserve_raw(
        &self,
        object: Id,
        group: Id,
        worker: Worker,
    ) -> Result<(Authority, u64, Id)> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(Op::ReserveRaw(object, group, worker));
        }
        self.write(|c| {
            live(c, worker)?;
            permit_restart(c, worker.slot)?;
            check_new_name(c, object)?;
            if c.query_row(
                "SELECT EXISTS(SELECT 1 FROM names WHERE id=?)",
                [object],
                |r| r.get(0),
            )? {
                return Err(Error::Fenced);
            }
            let owner = match authority(c, group) {
                Ok(a) => a,
                Err(Error::Missing(_)) => create_group_at(c, group, worker)?,
                Err(e) => return Err(e),
            };
            if owner.worker != worker {
                return Err(Error::Fenced);
            }
            let revision = fence(c, group, owner, true)?;
            let token = Id::new();
            c.execute(
                "INSERT INTO group_installations(unit,slot,token) VALUES(?,?,?)",
                params![group, worker.slot, token],
            )?;
            c.execute(
                "INSERT INTO pending_raw_names(object,unit) VALUES(?,?)",
                params![object, group],
            )?;
            Ok((owner, revision, token))
        })
    }
    /// An accepted spawn's first persisted data and its globally unique name
    /// replace the provisional claim atomically; a late/canceled token is fenced.
    pub(crate) fn commit_raw(
        &self,
        owner: Authority,
        base: u64,
        token: Id,
        record: Record,
        claims: Vec<Manifest>,
    ) -> Result<u64> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(Op::CommitRaw(owner, base, token, record, claims));
        }
        self.write(|c| {
            let object=record.id;
            let group=record.group;
            if record.deleted || record.revision!=0 || c.execute("DELETE FROM group_installations WHERE unit=? AND slot=? AND token=? AND EXISTS(SELECT 1 FROM pending_raw_names WHERE object=? AND unit=?)",params![group,owner.worker.slot,token,object,group])? != 1 { return Err(Error::Fenced); }
            apply_checkpoint(c,group,owner,base,vec![record],&BTreeMap::from([(object,claims)]))
        })
    }
    pub(crate) fn reserve_install(&self, group: Id, owner: Authority, revision: u64) -> Result<Id> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(Op::ReserveInstall(group, owner, revision));
        }
        self.write(|c| {
            permit_restart(c, owner.worker.slot)?;
            if fence(c, group, owner, true)? != revision {
                return Err(Error::Fenced);
            }
            let token = Id::new();
            c.execute(
                "INSERT INTO group_installations(unit,slot,token) VALUES(?,?,?)",
                params![group, owner.worker.slot, token],
            )?;
            Ok(token)
        })
    }
    pub(crate) fn release_install(&self, group: Id, worker: Worker, token: Id) -> Result<()> {
        if let Some(remote) = self.remote_connection() {
            return remote.call(Op::ReleaseInstall(group, worker, token));
        }
        self.write(|c| {
            c.execute(
                "DELETE FROM group_installations WHERE unit=? AND slot=? AND token=?",
                params![group, worker.slot, token],
            )?;
            Ok(())
        })
    }
}

pub(super) fn check_new_name(c: &Connection, object: Id) -> Result<()> {
    let claim:Option<(Id,Id)>=c.prepare_cached("SELECT p.unit,i.token FROM pending_raw_names p JOIN group_installations i ON i.unit=p.unit WHERE p.object=?")?.query_row([object],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((group, token)) = claim {
        permit_write(c, group, token)?;
    }
    Ok(())
}
