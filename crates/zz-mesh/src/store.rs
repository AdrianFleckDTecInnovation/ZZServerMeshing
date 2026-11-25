use crate::authority_store::{Connection as Remote, Operation as Op};
use crate::*;
use bevy_ecs::prelude::Resource;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[derive(Clone, Resource)]
pub struct Store(Arc<Backend>);

enum Backend {
    Local(Mutex<Connection>),
    Network(Remote),
}

const STORE_FORMAT: i64 = 1;

fn has_current_format(c: &Connection) -> Result<bool> {
    let marked = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='zz_mesh_format')",
        [], |row| row.get::<_, bool>(0),
    )?;
    if !marked {
        return Ok(false);
    }
    let version: Option<i64> = c
        .query_row(
            "SELECT version FROM zz_mesh_format WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version {
        Some(STORE_FORMAT) => Ok(true),
        Some(other) => Err(invalid(format!(
            "unsupported mesh store format {other}, expected {STORE_FORMAT}"
        ))),
        None => Err(invalid("mesh store format marker is empty")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub group: Id,
    pub revision: u64,
    pub progress: Option<ActionKey>,
    pub records: Vec<Record>,
    pub required: Vec<Manifest>,
}
impl Snapshot {
    pub fn hash(&self) -> Result<Digest> {
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        hash.update(b"zz-mesh/group/v1");
        hash.update(&encode(&(
            self.group,
            self.revision,
            self.progress,
            self.records.len() as u64,
        ))?);
        hash.update(&encode(&self.required)?);
        for r in &self.records {
            hash.update(&encode(r)?);
        }
        Ok(Digest(hash.finish().as_ref().try_into().unwrap()))
    }
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lookup {
    Unknown,
    Deleted,
    Present(Record),
}

pub(crate) fn authority(c: &Connection, group: Id) -> Result<Authority> {
    Ok(
        c.prepare_cached("SELECT slot,inc,gen FROM units WHERE id=?")?
            .query_row([group], |r| {
                Ok(Authority {
                    worker: Worker {
                        slot: r.get(0)?,
                        incarnation: r.get(1)?,
                    },
                    generation: r.get(2)?,
                })
            })
            .optional()?
            .ok_or_else(|| Error::Missing(format!("unit {group}")))?,
    )
}
pub(crate) fn live(c: &Connection, w: Worker) -> Result<()> {
    let ok: bool = c
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM workers WHERE slot=? AND incarnation=?)")?
        .query_row(params![w.slot, w.incarnation], |r| r.get(0))?;
    if ok {
        Ok(())
    } else {
        Err(Error::Fenced)
    }
}
pub(crate) fn fence(c: &Connection, g: Id, a: Authority, writing: bool) -> Result<u64> {
    live(c, a.worker)?;
    let row: Option<(u64, bool)> = c
        .prepare_cached(
            "SELECT rev,frozen IS NOT NULL FROM units WHERE id=? AND slot=? AND inc=? AND gen=?",
        )?
        .query_row(
            params![g, a.worker.slot, a.worker.incarnation, a.generation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((_, true)) if writing => Err(Error::Busy("unit frozen for cutover")),
        Some((rev, _)) => Ok(rev),
        None => Err(Error::Fenced),
    }
}
fn record(c: &Connection, id: Id) -> Result<Option<Record>> {
    let raw: Option<Vec<u8>> = c
        .prepare_cached("SELECT body FROM documents WHERE id=?")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    raw.map(|b| decode(&b)).transpose()
}
pub(crate) fn snapshot(c: &Connection, g: Id) -> Result<Snapshot> {
    let (revision, prog): (u64, Option<Vec<u8>>) =
        c.query_row("SELECT rev,progress FROM units WHERE id=?", [g], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let mut q = c.prepare("SELECT body FROM documents WHERE unit=? ORDER BY id")?;
    let mut records = Vec::new();
    for raw in q.query_map([g], |r| r.get::<_, Vec<u8>>(0))? {
        records.push(decode(&raw?)?);
    }
    let mut q=c.prepare("SELECT key,manifest FROM requirements WHERE unit=?1 UNION SELECT key,manifest FROM member_data WHERE unit=?1 ORDER BY key")?;
    let mut required = Vec::new();
    for b in q.query_map([g], |r| r.get::<_, Vec<u8>>(1))? {
        let m: Manifest = decode(&b?)?;
        if required
            .last()
            .is_some_and(|old: &Manifest| old.key == m.key)
        {
            return Err(invalid("conflicting dataset descriptors"));
        }
        required.push(m);
    }
    Ok(Snapshot {
        group: g,
        revision,
        records,
        required,
        progress: prog.map(|b| decode(&b)).transpose()?,
    })
}
impl Store {
    /// Open or initialize a persistent world. The first open of an unmarked v1
    /// store records its format atomically, keeping existing IDs and revisions.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut c = Connection::open(path)?;
        c.busy_timeout(Duration::from_secs(5))?;
        c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        if !has_current_format(&c)? {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !has_current_format(&tx)? {
                tx.execute_batch(include_str!("schema.sql"))?;
                tx.execute(
                    "INSERT INTO zz_mesh_format(singleton,version) VALUES(1,?)",
                    [STORE_FORMAT],
                )?;
            }
            tx.commit()?;
        }
        c.execute_batch("PRAGMA journal_mode=WAL;")?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS draining_slots(slot INTEGER PRIMARY KEY)")?;
        Ok(Self(Arc::new(Backend::Local(Mutex::new(c)))))
    }
    /// Connect a trusted worker to the authoritative volume via mutually authenticated TLS.
    pub fn connect(
        addr: std::net::SocketAddr,
        server_name: String,
        ca: Vec<u8>,
        credential: wire::Identity,
    ) -> Self {
        Self(Arc::new(Backend::Network(Remote::new(
            addr,
            server_name,
            ca,
            credential,
        ))))
    }
    pub(crate) fn remote_connection(&self) -> Option<&Remote> {
        match &*self.0 {
            Backend::Network(c) => Some(c),
            Backend::Local(_) => None,
        }
    }
    /// Cumulative TLS and payload counters for this worker's authority link, including reconnects.
    pub fn network_traffic(&self) -> Option<Arc<wire::Traffic>> {
        self.remote_connection().map(|c| c.traffic.clone())
    }
    /// Operator inventory of advertised current incarnations. The controller probes liveness over TLS.
    pub fn advertised_workers(&self) -> Result<Vec<(Worker, std::net::SocketAddr, bool)>> {
        let c = self.lock()?;
        let mut q = c.prepare("SELECT w.slot,w.incarnation,a.address,d.slot IS NOT NULL FROM workers w JOIN addresses a ON a.slot=w.slot AND a.inc=w.incarnation LEFT JOIN draining_slots d ON d.slot=w.slot ORDER BY w.slot")?;
        let rows = q.query_map([], |r| {
            Ok((
                Worker {
                    slot: r.get(0)?,
                    incarnation: r.get(1)?,
                },
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (w, a, draining) = row?;
            result.push((
                w,
                a.parse().map_err(|_| invalid("stored worker address"))?,
                draining,
            ));
        }
        Ok(result)
    }
    /// Persistently exclude a slot from new ownership. Cancel inbound preparations
    /// and release their frozen sources in the same transaction as the drain decision.
    pub fn set_draining(&self, w: Worker, draining: bool) -> Result<()> {
        self.write(|c| {
            live(c, w)?;
            if draining {
                c.execute("INSERT OR IGNORE INTO draining_slots VALUES(?)", [w.slot])?;
                let target = encode(&w)?;
                c.execute("UPDATE units SET frozen=NULL WHERE frozen IN (SELECT ticket FROM handoffs WHERE target=? AND phase<3)", [&target])?;
                c.execute("UPDATE handoffs SET phase=4 WHERE target=? AND phase<3", [&target])?;
            } else { c.execute("DELETE FROM draining_slots WHERE slot=?", [w.slot])?; }
            Ok(())
        })
    }
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        let Backend::Local(conn) = &*self.0 else {
            return Err(invalid("operation requires the local authority store"));
        };
        conn.lock().map_err(|_| invalid("storage mutex poisoned"))
    }
    pub(crate) fn check_authority(
        &self,
        group: Id,
        owner: Authority,
        writing: bool,
    ) -> Result<u64> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Fence(group, owner, writing));
        }
        fence(&*self.lock()?, group, owner, writing)
    }
    /// Resolve a bounded peer batch against one consistent authority snapshot.
    pub(crate) fn authorities(&self, ids: &[Id]) -> Result<BTreeMap<Id, (Id, Authority)>> {
        if ids.len() > MAX_TARGETS {
            return Err(Error::Busy("authority batch"));
        }
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Authorities(ids.to_vec()));
        }
        let conn = self.lock()?;
        let params = vec!["?"; ids.len()].join(",");
        let mut stmt = conn.prepare_cached(&format!("SELECT n.id,u.id,u.slot,u.inc,u.gen FROM names n JOIN units u ON u.id=n.unit WHERE n.id IN ({params})"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
            Ok((
                row.get(0)?,
                (
                    row.get(1)?,
                    Authority {
                        worker: Worker {
                            slot: row.get(2)?,
                            incarnation: row.get(3)?,
                        },
                        generation: row.get(4)?,
                    },
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
    /// Start the next incarnation of a one-based slot, fencing its previous writer.
    /// Call this once per worker process and reuse the returned identity.
    pub fn start_worker(&self, slot: u32) -> Result<Worker> {
        self.start_slot(slot, None)
    }
    /// Bind the new process certificate and fence the previous one in the same transaction.
    pub fn start_authenticated(&self, slot: u32, certificate: Digest) -> Result<Worker> {
        self.start_slot(slot, Some(certificate))
    }
    fn start_slot(&self, slot: u32, certificate: Option<Digest>) -> Result<Worker> {
        if slot == 0 {
            return Err(invalid("worker slots start at one"));
        }
        self.write(|c| {
            let incarnation=c.query_row("INSERT INTO workers VALUES(?,1) ON CONFLICT(slot) DO UPDATE SET incarnation=incarnation+1 RETURNING incarnation",[slot],|r|r.get(0))?;
            if let Some(cert) = certificate {
                c.execute("INSERT OR REPLACE INTO credentials VALUES(?,?,?)",
                    params![slot, incarnation, cert.0.as_slice()])?;
            }
            Ok(Worker{slot,incarnation})
        })
    }
    pub fn advertise(&self, w: Worker, addr: std::net::SocketAddr) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Advertise(w, addr));
        }
        self.write(|c| {
            live(c, w)?;
            c.execute(
                "INSERT OR REPLACE INTO addresses VALUES(?,?,?)",
                params![w.slot, w.incarnation, addr.to_string()],
            )?;
            Ok(())
        })
    }
    pub fn address(&self, w: Worker) -> Result<std::net::SocketAddr> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Address(w));
        }
        let c = self.lock()?;
        live(&c, w)?;
        let address: String = c
            .query_row(
                "SELECT address FROM addresses WHERE slot=? AND inc=?",
                params![w.slot, w.incarnation],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::Missing("worker address".into()))?;
        address
            .parse()
            .map_err(|_| invalid("stored worker address"))
    }
    pub fn worker(&self, slot: u32) -> Result<Worker> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Worker(slot));
        }
        Ok(Worker {
            slot,
            incarnation: self.lock()?.query_row(
                "SELECT incarnation FROM workers WHERE slot=?",
                [slot],
                |r| r.get(0),
            )?,
        })
    }
    pub fn owned(&self, slot: u32) -> Result<Vec<Id>> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Owned(slot));
        }
        let c = self.lock()?;
        let mut q = c.prepare("SELECT id FROM units WHERE slot=? ORDER BY id")?;
        let rows = q
            .query_map([slot], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn check_worker(&self, w: Worker) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Live(w));
        }
        live(&*self.lock()?, w)
    }
    pub fn bind_identity(&self, worker: Worker, cert: Digest) -> Result<()> {
        self.write(|c| {
            live(c, worker)?;
            c.execute(
                "INSERT OR REPLACE INTO credentials VALUES(?,?,?)",
                params![worker.slot, worker.incarnation, cert.0.as_slice()],
            )?;
            Ok(())
        })
    }
    pub fn authenticated_worker(&self, cert: Digest) -> Result<Worker> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Authenticated(cert));
        }
        let c = self.lock()?;
        let w = c
            .query_row(
                "SELECT slot,incarnation FROM credentials WHERE fingerprint=?",
                [cert.0.as_slice()],
                |r| {
                    Ok(Worker {
                        slot: r.get(0)?,
                        incarnation: r.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or(Error::Denied)?;
        live(&c, w)?;
        Ok(w)
    }
    pub fn create_group(&self, g: Id, w: Worker) -> Result<Authority> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Create(g, w));
        }
        self.write(|c| {
            live(c, w)?;
            accepting(c, w)?;
            c.execute(
                "INSERT INTO units(id,slot,inc,gen) VALUES(?,?,?,1)",
                params![g, w.slot, w.incarnation],
            )?;
            authority(c, g)
        })
    }
    pub fn owner(&self, g: Id) -> Result<Authority> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Owner(g));
        }
        authority(&*self.lock()?, g)
    }
    pub fn group_of(&self, id: Id) -> Result<Option<Id>> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Group(id));
        }
        Ok(self
            .lock()?
            .prepare_cached("SELECT unit FROM names WHERE id=?")?
            .query_row([id], |r| r.get(0))
            .optional()?)
    }
    pub(crate) fn notices(&self, w: Worker, since: u64) -> Result<Vec<(u64, Id, Authority, u64)>> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Notices(w, since));
        }
        let c = self.lock()?;
        let mut q=c.prepare_cached("SELECT n.mark,u.id,u.slot,u.inc,u.gen,u.rev FROM notices n JOIN units u ON u.id=n.unit WHERE n.slot=? AND n.mark>? ORDER BY n.mark LIMIT 128")?;
        let rows = q.query_map(params![w.slot, since], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                Authority {
                    worker: Worker {
                        slot: r.get(2)?,
                        incarnation: r.get(3)?,
                    },
                    generation: r.get(4)?,
                },
                r.get(5)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub(crate) fn since(&self, g: Id, base: u64) -> Result<(u64, Vec<Record>)> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Since(g, base));
        }
        self.since_for(g, base, None)
    }
    pub(crate) fn since_for(
        &self,
        g: Id,
        base: u64,
        reader: Option<Worker>,
    ) -> Result<(u64, Vec<Record>)> {
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        check_reader(&tx, g, reader)?;
        let (rev, floor): (u64, u64) =
            tx.query_row("SELECT rev,floor FROM units WHERE id=?", [g], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        if base < floor {
            return Err(invalid("checkpoint floor requires full snapshot"));
        }
        let mut rows = Vec::new();
        {
            let mut q =
                tx.prepare("SELECT body FROM documents WHERE unit=? AND rev>? ORDER BY id")?;
            for row in q.query_map(params![g, base], |r| r.get::<_, Vec<u8>>(0))? {
                rows.push(decode(&row?)?);
            }
        }
        {
            let mut q=tx.prepare("SELECT d.id,d.rev FROM departures d WHERE d.unit=? AND d.rev>? AND NOT EXISTS(SELECT 1 FROM documents r WHERE r.id=d.id AND r.unit=d.unit) ORDER BY d.id")?;
            for entry in q.query_map(params![g, base], |r| Ok((r.get(0)?, r.get(1)?)))? {
                let (id, revision) = entry?;
                rows.push(Record {
                    id,
                    group: g,
                    revision,
                    deleted: true,
                    fields: Default::default(),
                });
            }
        }
        tx.commit()?;
        Ok((rev, rows))
    }
    /// A restarted process may recover its own slot. An active transfer is cancelled atomically.
    pub fn recover(&self, g: Id, w: Worker) -> Result<Authority> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Recover(g, w));
        }
        self.write(|c| {
            live(c, w)?;
            let old = authority(c, g)?;
            if old.worker.slot != w.slot {
                return Err(Error::Denied);
            }
            if old.worker == w {
                return Ok(old);
            }
            c.execute("UPDATE handoffs SET phase=4 WHERE unit=? AND phase<3", [g])?;
            c.execute(
                "UPDATE units SET inc=?,gen=gen+1,frozen=NULL WHERE id=?",
                params![w.incarnation, g],
            )?;
            authority(c, g)
        })
    }
    pub fn lookup(&self, id: Id) -> Result<Lookup> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Lookup(id));
        }
        self.lookup_for(id, None)
    }
    pub(crate) fn lookup_for(&self, id: Id, reader: Option<Worker>) -> Result<Lookup> {
        let mut conn = self.lock()?;
        let c = conn.transaction()?;
        let result = match record(&c, id)? {
            Some(r) if !r.deleted && reader.is_some() => {
                check_reader(&c, r.group, reader)?;
                Lookup::Present(r)
            }
            Some(r) if !r.deleted => Lookup::Present(r),
            Some(_) => Lookup::Deleted,
            None => {
                if c.query_row("SELECT EXISTS(SELECT 1 FROM names WHERE id=?)", [id], |r| {
                    r.get::<_, bool>(0)
                })? {
                    Lookup::Deleted
                } else {
                    Lookup::Unknown
                }
            }
        };
        c.commit()?;
        Ok(result)
    }
    pub fn snapshot(&self, g: Id) -> Result<Snapshot> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Snapshot(g));
        }
        self.snapshot_for(g, None)
    }
    pub(crate) fn snapshot_for(&self, g: Id, reader: Option<Worker>) -> Result<Snapshot> {
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        check_reader(&tx, g, reader)?;
        let s = snapshot(&tx, g)?;
        tx.commit()?;
        Ok(s)
    }
    /// Compare and commit a checkpoint or initial group members. New records start at revision zero.
    pub fn checkpoint(&self, g: Id, a: Authority, base: u64, records: Vec<Record>) -> Result<u64> {
        self.checkpoint_data(g, a, base, records, &BTreeMap::new())
    }
    pub(crate) fn checkpoint_data(
        &self,
        g: Id,
        a: Authority,
        base: u64,
        records: Vec<Record>,
        claims: &BTreeMap<Id, Vec<Manifest>>,
    ) -> Result<u64> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Checkpoint(g, a, base, records, claims.clone()));
        }
        self.write(|c| {
            let rev = fence(c, g, a, true)?;
            if rev != base {
                return Err(Error::Fenced);
            }
            if records.len() > MAX_TARGETS {
                return Err(Error::Busy("checkpoint targets"));
            }
            let mut seen = BTreeSet::new();
            for r in records {
                if r.group != g || !seen.insert(r.id) {
                    return Err(invalid("wrong group or repeated object"));
                }
                if let Some(manifests) = claims.get(&r.id) {
                    if manifests.len() > MAX_TARGETS {
                        return Err(Error::Busy("member datasets"));
                    }
                    c.execute("DELETE FROM member_data WHERE object=?", [r.id])?;
                    for m in manifests {
                        c.execute(
                            "INSERT INTO member_data VALUES(?,?,?,?)",
                            params![r.id, g, encode(&m.key)?, encode(m)?],
                        )?;
                    }
                }
                put(c, r, rev + 1)?;
            }
            c.prepare_cached("UPDATE units SET rev=rev+1 WHERE id=?")?
                .execute([g])?;
            Ok(rev + 1)
        })
    }
    /// Atomically reassign an existing member and optionally its private data dependencies.
    /// Each participant supplies its expected generation and observed group revision.
    pub fn regroup(
        &self,
        id: Id,
        from: (Id, Authority, u64),
        to: (Id, Authority, u64),
        datasets: &[DataKey],
    ) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Regroup(id, from, to, datasets.to_vec()));
        }
        self.write(|c| {
            if from.0 == to.0 || datasets.len() > MAX_TARGETS {
                return Err(invalid("regroup participants or data count"));
            }
            if fence(c, from.0, from.1, true)? != from.2 || fence(c, to.0, to.1, true)? != to.2 {
                return Err(Error::Fenced);
            }
            let mut r = record(c, id)?.ok_or_else(|| Error::Missing(id.to_string()))?;
            if r.group != from.0 || r.deleted {
                return Err(Error::Fenced);
            }
            r.group = to.0;
            r.revision = to.2 + 1;
            c.execute(
                "UPDATE documents SET unit=?,rev=?,body=? WHERE id=?",
                params![r.group, r.revision, encode(&r)?, id],
            )?;
            c.execute("UPDATE names SET unit=? WHERE id=?", params![r.group, id])?;
            c.execute(
                "INSERT OR REPLACE INTO departures VALUES(?,?,?)",
                params![from.0, id, from.2 + 1],
            )?;
            c.execute(
                "DELETE FROM departures WHERE unit=? AND id=?",
                params![to.0, id],
            )?;
            c.execute(
                "UPDATE member_data SET unit=? WHERE object=?",
                params![to.0, id],
            )?;
            for d in datasets {
                let key = encode(d)?;
                let m: Vec<u8> = c.query_row(
                    "SELECT manifest FROM requirements WHERE unit=? AND key=?",
                    params![from.0, key],
                    |r| r.get(0),
                )?;
                c.execute(
                    "INSERT OR REPLACE INTO requirements VALUES(?,?,?)",
                    params![to.0, key, m],
                )?;
                c.execute(
                    "DELETE FROM requirements WHERE unit=? AND key=?",
                    params![from.0, key],
                )?;
            }
            c.execute(
                "UPDATE units SET rev=rev+1 WHERE id IN (?,?)",
                params![from.0, to.0],
            )?;
            Ok(())
        })
    }
    pub fn compact(&self, g: Id, a: Authority, through: u64) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Compact(g, a, through));
        }
        self.write(|c|{let rev=fence(c,g,a,true)?;if through>rev{return Err(Error::Fenced)}
            c.execute("DELETE FROM documents WHERE unit=? AND deleted=1 AND rev<=?",params![g,through])?;
            c.execute("DELETE FROM departures WHERE unit=? AND rev<=?",params![g,through])?;
            c.execute("UPDATE units SET floor=MAX(floor,?) WHERE id=?",params![through,g])?;
            c.execute("DELETE FROM handoffs WHERE unit=? AND phase>=3 AND ticket NOT IN (SELECT ticket FROM handoffs WHERE unit=? ORDER BY rowid DESC LIMIT 16)",params![g,g])?;Ok(())
        })
    }
    pub fn session(&self, session: Id, actor: Id) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Session(session, actor));
        }
        self.write(|c| {
            c.execute(
                "INSERT OR IGNORE INTO sessions(id,actor) VALUES(?,?)",
                params![session, actor],
            )?;
            let owner: Id =
                c.query_row("SELECT actor FROM sessions WHERE id=?", [session], |r| {
                    r.get(0)
                })?;
            if owner != actor {
                return Err(Error::Denied);
            }
            Ok(())
        })
    }
    pub fn progress(&self, session: Id, actor: Id, group: Id, a: Authority) -> Result<(u64, u64)> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Progress(session, actor, group, a));
        }
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let base = fence(&tx, group, a, false)?;
        let seq = tx
            .query_row(
                "SELECT last FROM sessions WHERE id=? AND actor=?",
                params![session, actor],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::Denied)?;
        tx.commit()?;
        Ok((base, seq))
    }
    pub fn requires(&self, group: Id, m: &Manifest) -> Result<bool> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Requires(group, m.clone()));
        }
        let c = self.lock()?;
        Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM requirements WHERE unit=?1 AND key=?2 AND manifest=?3 UNION ALL SELECT 1 FROM member_data WHERE unit=?1 AND key=?2 AND manifest=?3)",params![group,encode(&m.key)?,encode(m)?],|r|r.get(0))?)
    }
    pub fn requirements(&self, g: Id, a: Authority, base: u64, items: &[Manifest]) -> Result<u64> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Requirements(g, a, base, items.to_vec()));
        }
        if items.len() > MAX_TARGETS {
            return Err(Error::Busy("required datasets"));
        }
        self.write(|c| {
            if fence(c, g, a, true)? != base {
                return Err(Error::Fenced);
            }
            c.execute("DELETE FROM requirements WHERE unit=?", [g])?;
            for m in items {
                c.execute(
                    "INSERT INTO requirements VALUES(?,?,?)",
                    params![g, encode(&m.key)?, encode(m)?],
                )?;
            }
            c.execute("UPDATE units SET rev=rev+1 WHERE id=?", [g])?;
            Ok(base + 1)
        })
    }
    /// The remote writer endorses this exact operation at its current revision.
    pub fn approve(&self, g: Id, a: Authority, intent: &Intent) -> Result<()> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Approve(g, a, intent.clone()));
        }
        self.write(|c| {
            let rev = fence(c, g, a, true)?;
            let fingerprint = digest(&encode(intent)?);
            let pending: u32 =
                c.query_row("SELECT count(*) FROM permits WHERE unit=?", [g], |r| {
                    r.get(0)
                })?;
            c.execute(
                "DELETE FROM permits WHERE unit=? AND (rev<>? OR inc<>? OR gen<>?)",
                params![g, rev, a.worker.incarnation, a.generation],
            )?;
            if pending >= 128
                && c.query_row("SELECT count(*) FROM permits WHERE unit=?", [g], |r| {
                    r.get::<_, u32>(0)
                })? >= 128
            {
                return Err(Error::Busy("remote writer approvals"));
            }
            c.execute(
                "INSERT OR REPLACE INTO permits VALUES(?,?,?,?,?,?,?,?)",
                params![
                    g,
                    intent.key.session,
                    intent.key.seq,
                    fingerprint.0.as_slice(),
                    a.worker.slot,
                    a.worker.incarnation,
                    a.generation,
                    rev
                ],
            )?;
            Ok(())
        })
    }
    /// One SQLite group commit. Every decided action has its own durable outcome.
    pub fn execute(
        &self,
        registry: &Actions,
        principal: Id,
        writer: Authority,
        now: u64,
        batch: &[Intent],
    ) -> Result<Vec<Reply>> {
        if let Some(c) = self.remote_connection() {
            return c.call(Op::Execute(principal, writer, now, batch.to_vec()));
        }
        if batch.len() > MAX_BATCH || encode(&batch)?.len() > MAX_FRAME {
            return Err(Error::Busy("action batch"));
        }
        self.write(|c| {
            let mut replies = Vec::with_capacity(batch.len());
            for intent in batch {
                replies.push(decide(c, registry, principal, writer, now, intent)?);
            }
            Ok(replies)
        })
    }
}
fn check_reader(c: &Connection, group: Id, reader: Option<Worker>) -> Result<()> {
    if let Some(w) = reader {
        live(c, w)?;
        if authority(c, group)?.worker != w {
            return Err(Error::Denied);
        }
    }
    Ok(())
}
pub(crate) fn accepting(c: &Connection, w: Worker) -> Result<()> {
    if c.prepare_cached("SELECT EXISTS(SELECT 1 FROM draining_slots WHERE slot=?)")?
        .query_row([w.slot], |r| r.get::<_, bool>(0))?
    {
        return Err(Error::Busy("worker is draining"));
    }
    Ok(())
}
fn put(c: &Connection, mut r: Record, rev: u64) -> Result<()> {
    if r.fields.len() > 128 || r.fields.values().any(|f| f.bytes.len() > MAX_FIELD) {
        return Err(Error::Busy("record fields"));
    }
    match record(c, r.id)? {
        Some(old) if old.deleted || old.group != r.group || old.revision != r.revision => {
            return Err(Error::Fenced)
        }
        None => {
            if r.revision != 0 || r.deleted {
                return Err(Error::Fenced);
            }
            if c.execute(
                "INSERT OR IGNORE INTO names VALUES(?,?)",
                params![r.id, r.group],
            )? != 1
            {
                return Err(Error::Fenced);
            }
        }
        _ => {}
    }
    r.revision = rev;
    if r.deleted {
        c.execute("DELETE FROM member_data WHERE object=?", [r.id])?;
        r.fields.clear()
    }
    c.prepare_cached("INSERT INTO documents VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET rev=excluded.rev,deleted=excluded.deleted,body=excluded.body")?.execute(
        params![r.id,r.group,rev,r.deleted,encode(&r)?])?;
    Ok(())
}

pub trait Action: Serialize + DeserializeOwned + Send + Sync + 'static {
    const ID: u32;
    const VERSION: u16;
    type Output: Serialize + DeserializeOwned;
}
type Handler = Box<dyn Fn(&mut Edit<'_>, &[u8]) -> Result<Vec<u8>> + Send + Sync>;
#[derive(Default, Resource)]
pub struct Actions(BTreeMap<u32, (u16, Handler)>);
impl Actions {
    pub fn register<A: Action>(
        &mut self,
        handler: fn(&mut Edit<'_>, A) -> Result<A::Output>,
    ) -> Result<()> {
        if self.0.contains_key(&A::ID) {
            return Err(invalid(format!("duplicate action schema {}", A::ID)));
        }
        self.0.insert(
            A::ID,
            (
                A::VERSION,
                Box::new(move |edit, bytes| encode(&handler(edit, decode(bytes)?)?)),
            ),
        );
        Ok(())
    }
}
/// Restricted transaction overlay. Reads see earlier edits in the same action.
pub struct Edit<'a> {
    c: &'a Connection,
    pub actor: Id,
    pub now: u64,
    pub group: Id,
    pub key: ActionKey,
    changes: BTreeMap<Id, Record>,
    datasets: BTreeMap<(Id, DataKey), Manifest>,
}
impl Edit<'_> {
    pub fn read(&self, id: Id) -> Result<Record> {
        self.changes
            .get(&id)
            .cloned()
            .map(Some)
            .map(Ok)
            .unwrap_or_else(|| record(self.c, id))?
            .filter(|r| !r.deleted)
            .ok_or_else(|| Error::Missing(format!("object {id}")))
    }
    pub fn save(&mut self, r: Record) -> Result<()> {
        if self.changes.len() >= MAX_TARGETS && !self.changes.contains_key(&r.id) {
            return Err(Error::Busy("atomic change set"));
        }
        self.changes.insert(r.id, r);
        Ok(())
    }
    pub fn delete(&mut self, id: Id) -> Result<()> {
        let mut r = self.read(id)?;
        r.deleted = true;
        self.save(r)
    }
    pub fn require(&mut self, group: Id, m: Manifest) -> Result<()> {
        if self.datasets.len() >= MAX_TARGETS {
            return Err(Error::Busy("datasets per change set"));
        }
        self.datasets.insert((group, m.key), m);
        Ok(())
    }
}
fn decide(
    c: &Connection,
    reg: &Actions,
    actor: Id,
    a: Authority,
    now: u64,
    i: &Intent,
) -> Result<Reply> {
    let id = i.key.session;
    let seq = i.key.seq;
    let session: Option<(Id, u64)> = c
        .query_row("SELECT actor,last FROM sessions WHERE id=?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((owner, last)) = session else {
        return Err(Error::Denied);
    };
    if owner != actor {
        return Err(Error::Denied);
    }
    let fingerprint = digest(&encode(i)?);
    if seq <= last {
        let old: Option<(Vec<u8>, Vec<u8>)> = c
            .query_row(
                "SELECT request,receipt FROM results WHERE session=? AND seq=?",
                params![id, seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        return match old {
            Some((hash, data)) if hash == fingerprint.0 => Ok(Reply::Decided(decode(&data)?)),
            Some(_) => Err(invalid("action ID reused for different intent")),
            None => Ok(Reply::Resync(last)),
        };
    }
    if seq != last + 1 {
        return Ok(Reply::Expected(last + 1));
    }
    let base = match fence(c, i.group, a, true) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::Retry(e.to_string())),
    };
    let mut e = Edit {
        c,
        actor,
        now,
        group: i.group,
        key: i.key,
        changes: BTreeMap::new(),
        datasets: BTreeMap::new(),
    };
    let evaluated = (|| {
        if let Some(dep) = i.after {
            let receipt: Option<Vec<u8>> = c
                .query_row(
                    "SELECT receipt FROM results WHERE session=? AND seq=?",
                    params![id, dep],
                    |r| r.get(0),
                )
                .optional()?;
            if !matches!(
                receipt
                    .map(|x| decode::<Receipt>(&x))
                    .transpose()?
                    .map(|r| r.outcome),
                Some(Outcome::Committed(_))
            ) {
                return Err(invalid("dependency unconfirmed or rejected"));
            }
        }
        let (version, run) = reg
            .0
            .get(&i.kind)
            .ok_or_else(|| invalid(format!("unknown action {}", i.kind)))?;
        if *version != i.version {
            return Err(invalid(format!("unsupported action version {}", i.version)));
        }
        run(&mut e, &i.bytes)
    })();
    let mut versions = BTreeMap::from([(i.group, base + 1)]);
    let outcome = match evaluated {
        Err(Error::Sql(err)) => return Err(err.into()),
        Err(err) => Outcome::Rejected(err.to_string()),
        Ok(result) => {
            if result.len() > MAX_FIELD {
                return Err(Error::Busy("action result"));
            }
            for group in e
                .changes
                .values()
                .map(|r| r.group)
                .chain(e.datasets.keys().map(|k| k.0))
            {
                if versions.contains_key(&group) {
                    continue;
                }
                let permit:Option<(u32,u64,u64,u64)>=c.query_row("SELECT slot,inc,gen,rev FROM permits WHERE unit=? AND session=? AND seq=? AND request=?",
                  params![group,id,seq,fingerprint.0.as_slice()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                let Some((slot, incarnation, generation, rev)) = permit else {
                    return Ok(Reply::Retry(format!("waiting for writer of {group}")));
                };
                if fence(
                    c,
                    group,
                    Authority {
                        worker: Worker { slot, incarnation },
                        generation,
                    },
                    true,
                )
                .ok()
                    != Some(rev)
                {
                    return Ok(Reply::Retry("remote writer changed".into()));
                }
                versions.insert(group, rev + 1);
            }
            for (_, r) in e.changes {
                let rev = versions[&r.group];
                put(c, r, rev)?;
            }
            for ((g, k), m) in e.datasets {
                let key = encode(&k)?;
                let bytes = encode(&m)?;
                let count = c.execute(
                    "UPDATE member_data SET manifest=? WHERE unit=? AND key=?",
                    params![bytes, g, key],
                )?;
                if count == 0 {
                    c.execute(
                        "INSERT OR REPLACE INTO requirements VALUES(?,?,?)",
                        params![g, key, bytes],
                    )?;
                }
            }
            Outcome::Committed(result)
        }
    };
    for (g, r) in versions {
        c.execute(
            "UPDATE units SET rev=?,progress=? WHERE id=?",
            params![r, encode(&i.key)?, g],
        )?;
    }
    let receipt = Receipt {
        key: i.key,
        revision: base + 1,
        outcome,
    };
    c.execute(
        "INSERT INTO results VALUES(?,?,?,?)",
        params![id, seq, fingerprint.0.as_slice(), encode(&receipt)?],
    )?;
    c.execute("UPDATE sessions SET last=? WHERE id=?", params![seq, id])?;
    c.execute(
        "DELETE FROM results WHERE session=? AND seq<=?",
        params![id, seq.saturating_sub(RECEIPTS)],
    )?;
    c.execute(
        "DELETE FROM permits WHERE session=? AND seq<=?",
        params![id, seq],
    )?;
    Ok(Reply::Decided(receipt))
}
