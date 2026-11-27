//! Operator process for placement across already provisioned network workers.
use super::*;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn tick() -> Result<u64> {
    Ok((SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("controller clock"))?
        .as_millis()
        / 50) as u64)
}
struct Peer {
    address: SocketAddr,
    cap: Capacity,
    link: Client,
}
fn load(link: &mut Client, group: Option<Id>, required: Vec<Manifest>) -> Result<WorkerLoad> {
    match link.call(&Request::Load { group, required })? {
        Response::Load(r) => Ok(r),
        Response::Error(e) => Err(invalid(e)),
        _ => Err(invalid("worker placement metrics unavailable")),
    }
}
fn one(dir: &Path, db: &Store, drain: Option<Worker>) -> Result<bool> {
    let policy = BalancePolicy::default();
    let control = control(dir)?;
    // A lost decision reply takes priority over a new placement decision.
    let pending: Option<Vec<u8>> = control
        .query_row(
            "SELECT ticket FROM pending ORDER BY slot LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(bytes) = pending {
        let t: Handoff = decode(&bytes)?;
        let owner = db.owner(t.group)?;
        match db.handoff_phase(t) {
            Ok(Phase::Switched) => {
                if owner.worker == t.target {
                    let mut target = connect(dir, db.address(t.target)?, "operator")?;
                    let answer =
                        target.call(&Request::Control(t, is_mesh::remote::Command::Activate))?;
                    if !matches!(answer, Response::Control(is_mesh::remote::Answer::Done)) {
                        return Err(invalid(format!("waiting for committed target: {answer:?}")));
                    }
                }
                control.execute(
                    "INSERT OR REPLACE INTO last_moves VALUES(?,?)",
                    rusqlite::params![t.group, tick()?],
                )?;
                forget(&control, t)?;
            }
            Ok(Phase::Aborted) => forget(&control, t)?,
            state => {
                let started = match state {
                    Ok(_) => true,
                    Err(Error::Missing(_)) => false,
                    Err(e) => return Err(e),
                };
                if owner != t.source || db.worker(t.target.slot)? != t.target {
                    if started && owner == t.source {
                        db.abort_handoff(t)?;
                    }
                    forget(&control, t)?;
                } else {
                    transfer(
                        dir,
                        db,
                        t,
                        db.address(t.source.worker)?,
                        db.address(t.target)?,
                        policy.transfer_bytes as usize,
                    )?;
                }
            }
        }
        return Ok(true);
    }
    let mut peers = BTreeMap::new();
    for (w, address, draining) in db.advertised_workers()? {
        let probe = (|| {
            let mut link = connect(dir, address, "operator")?;
            let r = load(&mut link, None, vec![])?;
            if r.worker != w {
                return Err(Error::Fenced);
            }
            Ok(Peer {
                address,
                link,
                cap: Capacity {
                    worker: w,
                    tick_us: r.tick_us,
                    budget_us: r.budget_us,
                    used: r.process.resident_bytes.max(r.cache_bytes),
                    memory: r.memory_bytes,
                    draining,
                },
            })
        })();
        match probe {
            Ok(peer) => {
                peers.insert(w, peer);
            }
            Err(err) if Some(w) != drain => {
                eprintln!("Worker {} unavailable for placement: {err}", w.slot)
            }
            Err(err) => return Err(err),
        }
    }
    let caps: Vec<_> = peers.values().map(|p| p.cap.clone()).collect();
    let now = tick()?;
    for cap in caps.iter().filter(|c| {
        drain.is_none_or(|d| c.worker == d)
            && (c.draining || c.tick_us > c.budget_us || c.used > c.memory)
    }) {
        let groups = db.owned(cap.worker.slot)?;
        for group in groups {
            let snap = db.snapshot(group)?;
            let meta = snap
                .records
                .iter()
                .flat_map(|r| r.fields.values())
                .map(|f| f.bytes.len() as u64)
                .sum::<u64>();
            let bytes = meta.saturating_add(snap.required.iter().map(|m| m.length).sum::<u64>());
            let source = peers.get_mut(&cap.worker).unwrap();
            let r = load(&mut source.link, Some(group), vec![])?;
            let mut g = GroupLoad {
                group,
                owner: cap.worker,
                cost_us: r.group_us.unwrap_or(cap.tick_us / r.groups.max(1) as u64),
                bytes,
                moved_at: control
                    .query_row("SELECT tick FROM last_moves WHERE unit=?", [group], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .unwrap_or(0),
                affinity: r.affinity,
                cached: BTreeMap::new(),
            };
            for (&w, peer) in &mut peers {
                if w == cap.worker || peer.cap.draining {
                    continue;
                }
                for batch in snap.required.chunks(MAX_TARGETS) {
                    *g.cached.entry(w).or_default() +=
                        load(&mut peer.link, None, batch.to_vec())?.cached_required;
                }
            }
            match place(&g, &caps, now, policy.dwell, policy.transfer_bytes) {
                Placement::Move(to) => {
                    let source = db.owner(group)?;
                    if source.worker != cap.worker { return Err(Error::Fenced); }
                    let t = Handoff{id:Id::new(),group,source,target:to};
                    transfer(dir, db, t, peers[&cap.worker].address, peers[&to].address, policy.transfer_bytes as usize)?;
                    return Ok(true);
                }
                Placement::Overloaded => eprintln!("Group {group} on slot {} has no feasible target for this tick, memory and transfer budget",cap.worker.slot),
                Placement::Stay => (),
            }
        }
    }
    Ok(false)
}
pub(super) fn balance(dir: &Path, once: bool) -> Result<()> {
    let db = Store::open(dir.join("world.sqlite"))?;
    loop {
        let result = one(dir, &db, None);
        if once {
            return result.map(|_| ());
        }
        if let Err(e) = result {
            eprintln!("Placement waiting: {e}");
        }
        std::thread::sleep(Duration::from_millis(BalancePolicy::default().every * 50));
    }
}
pub(super) fn drain(dir: &Path, slot: u32) -> Result<()> {
    let db = Store::open(dir.join("world.sqlite"))?;
    let w = db.worker(slot)?;
    db.set_draining(w, true)?;
    while !db.owned(slot)?.is_empty() {
        if !one(dir, &db, Some(w))? {
            return Err(Error::Busy("drain waiting for a feasible live worker"));
        }
    }
    println!("Slot {slot} drained");
    Ok(())
}
