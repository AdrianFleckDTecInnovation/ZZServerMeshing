//! Typed control/storage adapter for trusted workers on separate hosts.
//! Only the authority service opens the SQLite volume. Compatibility handlers prepare
//! outside the writer. Their validated edits and receipts commit atomically.
use crate::{
    wire::{Client, Endpoint, Identity, Request, Response, Role},
    *,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr, sync::Mutex};

type Claims = BTreeMap<Id, Vec<Manifest>>;
type Participant = (Id, Authority, u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operation {
    Advertise(Worker, SocketAddr),
    Address(Worker),
    Worker(u32),
    Owned(u32),
    Live(Worker),
    DrainStatus(Worker),
    Retire(Worker),
    Authenticated(Digest),
    Create(Id, Worker),
    Owner(Id),
    Group(Id),
    Notices(Worker, u64),
    Since(Id, u64),
    Recover(Id, Worker),
    Lookup(Id),
    Snapshot(Id),
    Fence(Id, Authority, bool),
    Checkpoint(Id, Authority, u64, Vec<Record>, Claims),
    Regroup(Id, Participant, Participant, Vec<DataKey>),
    Compact(Id, Authority, u64),
    Session(Id, Id),
    Progress(Id, Id, Id, Authority),
    Requires(Id, Manifest),
    Requirements(Id, Authority, u64, Vec<Manifest>),
    Approve(Id, Authority, Intent),
    Execute(Id, Authority, u64, Vec<Intent>),
    Begin(Handoff),
    Frozen(Handoff),
    Phase(Handoff),
    Freeze(Handoff, u64, Vec<Record>, Vec<Record>),
    Ready(Handoff, Worker, Digest),
    Switch(Handoff),
    Abort(Handoff),
    Authorities(Vec<Id>),
    GroupStates(Vec<Id>, Option<Worker>),
    PlayerCredential(Digest),
    ChangeControl(ControlChange),
    ReadControls(Vec<Id>),
    WorkerControls(Worker, Vec<(Id, Id, u64)>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Failure {
    Denied,
    Fenced,
    Missing(String),
    Busy,
    Invalid(String),
}
impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        match e {
            Error::Denied => Self::Denied,
            Error::Fenced => Self::Fenced,
            Error::Missing(s) => Self::Missing(s),
            Error::Busy(_) => Self::Busy,
            other => Self::Invalid(other.to_string()),
        }
    }
}
impl From<Failure> for Error {
    fn from(e: Failure) -> Self {
        match e {
            Failure::Denied => Self::Denied,
            Failure::Fenced => Self::Fenced,
            Failure::Missing(s) => Self::Missing(s),
            Failure::Busy => Self::Busy("authority service busy"),
            Failure::Invalid(s) => invalid(s),
        }
    }
}

pub(crate) struct Connection {
    addr: SocketAddr,
    name: String,
    ca: Vec<u8>,
    credential: Identity,
    pub(crate) traffic: std::sync::Arc<wire::Traffic>,
    socket: Mutex<Option<Client>>,
}
impl Connection {
    pub(crate) fn new(addr: SocketAddr, name: String, ca: Vec<u8>, credential: Identity) -> Self {
        Self {
            addr,
            name,
            ca,
            credential,
            traffic: Default::default(),
            socket: Mutex::default(),
        }
    }
    pub(crate) fn call<T: DeserializeOwned>(&self, op: Operation) -> Result<T> {
        let mut socket = self
            .socket
            .lock()
            .map_err(|_| invalid("authority connection lock"))?;
        use Operation::*;
        let read = matches!(
            op,
            Address(_)
                | Worker(_)
                | Owned(_)
                | Live(_)
                | DrainStatus(_)
                | Authenticated(_)
                | PlayerCredential(_)
                | ReadControls(_)
                | WorkerControls(..)
                | Owner(_)
                | Group(_)
                | Notices(..)
                | Since(..)
                | Lookup(_)
                | Snapshot(_)
                | Fence(..)
                | Progress(..)
                | Requires(..)
                | Frozen(_)
                | Phase(_)
                | Authorities(_)
                | GroupStates(..)
        );
        let connect = || {
            Client::connect_counted(
                self.addr,
                &self.name,
                &self.ca,
                &self.credential,
                self.traffic.clone(),
            )
        };
        let reused = socket.is_some();
        if !reused {
            *socket = Some(connect()?);
        }
        let request = Request::Storage(op);
        let mut answer = socket.as_mut().unwrap().call(&request);
        // Only reads may be transparently retried on a recycled/idle socket. Each
        // mutation has a receipt, expected revision or handoff ticket for recovery.
        if reused && read && matches!(answer, Err(Error::Io(_))) {
            *socket = None;
            *socket = Some(connect()?);
            answer = socket.as_mut().unwrap().call(&request);
        }
        match answer {
            Ok(Response::Stored(Ok(bytes))) => decode(&bytes),
            Ok(Response::Stored(Err(e))) => Err(e.into()),
            Ok(_) => Err(invalid("unexpected authority response")),
            Err(e) => {
                *socket = None;
                Err(e)
            }
        }
    }
}

/// Serve on a dedicated mTLS listener which resolves current worker incarnations.
pub struct AuthorityService {
    store: Store,
    actions: Actions,
}
impl AuthorityService {
    pub fn new(store: Store, actions: Actions) -> Result<Self> {
        if store.remote_connection().is_some() {
            return Err(invalid("authority service needs a local durable store"));
        }
        Ok(Self { store, actions })
    }
    fn execute(&self, who: Worker, op: Operation) -> Result<Vec<u8>> {
        let db = &self.store;
        db.check_worker(who)?;
        let own = |w: crate::Worker| if w == who { Ok(()) } else { Err(Error::Denied) };
        let party = |t: Handoff| {
            if t.source.worker == who || t.target == who {
                Ok(())
            } else {
                Err(Error::Denied)
            }
        };
        use Operation::*;
        match op {
            ChangeControl(change) => encode(&db.control_change(change)?),
            ReadControls(objects) => encode(&db.control_states(&objects)?),
            WorkerControls(worker, objects) => {
                own(worker)?;
                encode(&db.worker_controls(worker, &objects)?)
            }
            Advertise(w, a) => {
                own(w)?;
                encode(&db.advertise(w, a)?)
            }
            Address(w) => encode(&db.address(w)?),
            Worker(s) => encode(&db.worker(s)?),
            Owned(s) if s == who.slot => encode(&db.owned(s)?),
            Owned(_) => Err(Error::Denied),
            Live(w) => encode(&db.check_worker(w)?),
            DrainStatus(w) => {
                own(w)?;
                encode(&db.drain_status(w)?)
            }
            Retire(w) => {
                own(w)?;
                encode(&db.retire(w)?)
            }
            Authenticated(c) => encode(&db.authenticated_worker(c)?),
            PlayerCredential(c) => encode(&db.authenticated_player(c)?),
            Create(g, w) => {
                own(w)?;
                encode(&db.create_group(g, w)?)
            }
            Owner(g) => encode(&db.owner(g)?),
            Group(id) => encode(&db.group_of(id)?),
            Notices(w, n) => {
                own(w)?;
                encode(&db.notices(w, n)?)
            }
            Since(g, n) => encode(&db.since_for(g, n, Some(who))?),
            Recover(g, w) => {
                own(w)?;
                encode(&db.recover(g, w)?)
            }
            Lookup(id) => encode(&db.lookup_for(id, Some(who))?),
            Snapshot(g) => encode(&db.snapshot_for(g, Some(who))?),
            Fence(g, a, writing) => encode(&db.check_authority(g, a, writing)?),
            Checkpoint(g, a, base, records, claims) => {
                own(a.worker)?;
                encode(&db.checkpoint_data(g, a, base, records, &claims)?)
            }
            Regroup(id, from, to, data) => {
                own(from.1.worker)?;
                own(to.1.worker)?;
                encode(&db.regroup(id, from, to, &data)?)
            }
            Compact(g, a, mark) => {
                own(a.worker)?;
                encode(&db.compact(g, a, mark)?)
            }
            Session(id, actor) => encode(&db.session(id, actor)?),
            Progress(s, id, g, a) => {
                own(a.worker)?;
                encode(&db.progress(s, id, g, a)?)
            }
            Requires(g, m) => encode(&db.requires(g, &m)?),
            Requirements(g, a, base, data) => {
                own(a.worker)?;
                encode(&db.requirements(g, a, base, &data)?)
            }
            Approve(g, a, intent) => {
                own(a.worker)?;
                encode(&db.approve(g, a, &intent)?)
            }
            Execute(id, a, now, batch) => {
                own(a.worker)?;
                encode(&db.execute(&self.actions, id, a, now, &batch)?)
            }
            Begin(t) => {
                own(t.source.worker)?;
                encode(&db.begin_handoff(t)?)
            }
            Frozen(t) => {
                party(t)?;
                encode(&db.frozen(t)?)
            }
            Phase(t) => {
                party(t)?;
                encode(&db.handoff_phase(t)?)
            }
            Freeze(t, base, records, checkpoint) => {
                own(t.source.worker)?;
                encode(&db.freeze_checkpoint(t, base, &records, &checkpoint)?)
            }
            Ready(t, w, hash) => {
                own(w)?;
                encode(&db.acknowledge_hash(t, w, hash)?)
            }
            Switch(t) => {
                party(t)?;
                encode(&db.switch(t)?)
            }
            Abort(t) => {
                party(t)?;
                encode(&db.abort_handoff(t)?)
            }
            Authorities(ids) => encode(&db.authorities(&ids)?),
            GroupStates(ids, worker) => {
                if let Some(w) = worker {
                    own(w)?;
                }
                encode(&db.group_states(&ids, worker)?)
            }
        }
    }
}
impl Endpoint for AuthorityService {
    fn dispatch(&self, role: Role, request: Request) -> Result<Response> {
        match (role, request) {
            (Role::Worker(w), Request::Storage(op)) => {
                Ok(Response::Stored(self.execute(w, op).map_err(Into::into)))
            }
            _ => Err(Error::Denied),
        }
    }
}
