use infinite_stars::{host::*, prediction::*, *};
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use zz_mesh::{
    wire::{Client, Request, Response},
    *,
};
mod supervisor;

const LOCAL: &str = "127.0.0.1:8642";
const USAGE:&str="infinite-stars demo [DIR] | serve DIR SLOT ADDRESS | authority DIR ADDRESS | provision DIR SLOT NEW_DIR | provision-client DIR NEW_DIR [ACTOR_UUID] | worker DIR AUTHORITY_ADDRESS LISTEN_ADDRESS | client DIR ADDRESS | sight DIR WORKER_ADDRESS | migrate DIR SOURCE TARGET SLOT | balance DIR [--once] | drain DIR SLOT | resume DIR SLOT | abort DIR SLOT | status DIR ADDRESS";

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn argument<T: FromStr>(value: Option<String>, name: &str) -> Result<T> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| invalid(format!("expected {name}")))
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "demo".into());
    let dir = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/infinite-stars".into()),
    );
    fs::create_dir_all(&dir)?;
    match mode.as_str() {
        "demo" => demo(&dir),
        "balance" => {
            let once = match args.next().as_deref() {
                None => false,
                Some("--once") => true,
                _ => return Err(invalid("balance accepts --once")),
            };
            supervisor::balance(&dir, once)
        }
        "drain" => supervisor::drain(&dir, argument(args.next(), "worker slot")?),
        "resume" => {
            let db = Store::open(dir.join("world.sqlite"))?;
            db.set_draining(db.worker(argument(args.next(), "worker slot")?)?, false)
        }
        "authority" => deployment::serve(
            &dir,
            argument(args.next(), "authority listen address")?,
            args.next().map(PathBuf::from),
        ),
        "provision" => {
            let slot = argument(args.next(), "worker slot")?;
            let dest: PathBuf = argument(args.next(), "new credential directory")?;
            deployment::provision(&dir, &dest, Some(slot))
        }
        "provision-client" => {
            let dest: PathBuf = argument(args.next(), "new credential directory")?;
            if let Some(id) = args.next() {
                deployment::provision_player(&dir, &dest, argument(Some(id), "actor UUID")?)
            } else {
                deployment::provision(&dir, &dest, None)
            }
        }
        "sight" => {
            let addr: SocketAddr = argument(
                args.next().or_else(|| Some(LOCAL.into())),
                "bootstrap worker address",
            )?;
            let mut bootstrap = connect(&dir, addr, "player")?;
            let mut sight = PlayerSightClient::default();
            sight.refresh(&mut bootstrap, |_, source| connect(&dir, source, "player"))?;
            for (&id, r) in sight.records() {
                let pos: Position = r.get(POSITION, 1)?;
                let kind = if r.fields.contains_key(&PLAYER) {
                    "player"
                } else {
                    "ship"
                };
                println!("{id} {kind} {} {:?} mm", pos.frame, pos.mm);
            }
            println!("{} visible", sight.records().len());
            Ok(())
        }
        "worker" => deployment::worker(
            dir,
            argument(args.next(), "authority address")?,
            argument(args.next(), "worker listen address")?,
            args.next().map(PathBuf::from),
        ),
        "serve" => {
            let slot = argument(args.next().or_else(|| Some("1".into())), "worker slot")?;
            let addr = argument(args.next().or_else(|| Some(LOCAL.into())), "listen address")?;
            listen(dir, slot, addr, args.next().map(PathBuf::from))
        }
        "client" => client(
            &dir,
            argument(args.next().or_else(|| Some(LOCAL.into())), "entry address")?,
        ),
        "migrate" => migrate(
            &dir,
            argument(args.next(), "source address")?,
            argument(args.next(), "target address")?,
            argument(args.next(), "target slot")?,
        ),
        "abort" => abort(&dir, argument(args.next(), "target slot")?),
        "status" => status(
            &dir,
            argument(args.next().or_else(|| Some(LOCAL.into())), "worker address")?,
        ),
        _ => Err(invalid(USAGE)),
    }
}
fn demo(dir: &Path) -> Result<()> {
    let db = Store::open(dir.join("world.sqlite"))?;
    let slot = match db.owner(GROUP) {
        Ok(a) => a.worker.slot,
        Err(Error::Missing(_)) => 1,
        Err(e) => return Err(e),
    };
    let (source, app) = start_node(&db, slot)?;
    let target = db.start_worker(if slot == 1 { 2 } else { 1 })?;
    let mut pool = Pool::new(db.clone(), 1024);
    pool.add(source, app)?;
    pool.add(target, world(&db, target))?;
    pool.step()?;
    pool.deliver_all()?;
    let cost = pool.transfer(GROUP, target, 1024 * 1024)?;
    pool.step()?;
    pool.deliver_all()?;
    print_cost(cost);
    Ok(())
}
fn print_cost(cost: MigrationCost) {
    print_group_cost(GROUP, cost);
}
fn print_group_cost(group: Id, cost: MigrationCost) {
    println!(
        "Group {group} on worker {}, generation {}. {} bulk bytes, preparation {:?}, pause {:?}.",
        cost.authority.worker.slot,
        cost.authority.generation,
        cost.payload,
        cost.prepare,
        cost.pause
    );
}
fn connect(dir: &Path, addr: SocketAddr, credential: &str) -> Result<Client> {
    let name = match fs::read_to_string(dir.join("server-name")) {
        Ok(name) => name,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => if credential == "operator" {
            deployment::GAME_DNS
        } else {
            "localhost"
        }
        .to_owned(),
        Err(e) => return Err(e.into()),
    };
    Client::connect(
        addr,
        name.trim(),
        &fs::read(dir.join("ca.der"))?,
        &identity(dir, credential)?,
    )
}
fn control(dir: &Path) -> Result<Connection> {
    let c = Connection::open(dir.join("controller.sqlite"))?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS pending(slot INTEGER PRIMARY KEY, ticket BLOB NOT NULL); CREATE TABLE IF NOT EXISTS last_moves(unit BLOB PRIMARY KEY, tick INTEGER NOT NULL)")?;
    Ok(c)
}
fn forget(c: &Connection, t: Handoff) -> Result<()> {
    c.execute(
        "DELETE FROM pending WHERE slot=? AND ticket=?",
        rusqlite::params![t.target.slot, encode(&t)?],
    )?;
    Ok(())
}
fn migrate(dir: &Path, source: SocketAddr, target: SocketAddr, slot: u32) -> Result<()> {
    let db = Store::open(dir.join("world.sqlite"))?;
    let proposal = Handoff {
        id: Id::new(),
        group: GROUP,
        source: db.owner(GROUP)?,
        target: db.worker(slot)?,
    };
    transfer(dir, &db, proposal, source, target, 16 << 20)
}
fn transfer(
    dir: &Path,
    db: &Store,
    proposal: Handoff,
    source: SocketAddr,
    target: SocketAddr,
    budget: usize,
) -> Result<()> {
    let mut control = control(dir)?;
    let slot = proposal.target.slot;
    control.execute(
        "INSERT OR IGNORE INTO pending VALUES(?,?)",
        rusqlite::params![slot, encode(&proposal)?],
    )?;
    let bytes: Vec<u8> =
        control.query_row("SELECT ticket FROM pending WHERE slot=?", [slot], |row| {
            row.get(0)
        })?;
    let t: Handoff = decode(&bytes)?;
    if t.group != proposal.group {
        return Err(Error::Busy("another transfer uses the target slot"));
    }
    if matches!(db.handoff_phase(t), Ok(Phase::Switched)) && db.owner(t.group)?.worker != t.target {
        println!(
            "Transfer already committed. Current authority {:?}",
            db.owner(t.group)?
        );
        forget(&control, t)?;
        return Ok(());
    }
    let key = controller_credential(dir, t);
    let mut from = connect(dir, source, &key)?;
    let mut to = connect(dir, target, &key)?;
    let cost = zz_mesh::remote::migrate(&mut from, &mut to, t, budget)?;
    print_group_cost(t.group, cost);
    let tx = control.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO last_moves VALUES(?,?)",
        rusqlite::params![t.group, supervisor::tick()?],
    )?;
    forget(&tx, t)?;
    tx.commit()?;
    Ok(())
}
fn abort(dir: &Path, slot: u32) -> Result<()> {
    let control = control(dir)?;
    let bytes: Vec<u8> =
        control.query_row("SELECT ticket FROM pending WHERE slot=?", [slot], |r| {
            r.get(0)
        })?;
    let t: Handoff = decode(&bytes)?;
    let db = Store::open(dir.join("world.sqlite"))?;
    // Register an unopened ticket first so a delayed Preview cannot reopen it.
    if matches!(db.handoff_phase(t), Err(Error::Missing(_))) {
        db.begin_handoff(t)?;
    }
    let phase = db.abort_handoff(t)?;
    println!(
        "Durable decision {phase:?}. Current authority {:?}",
        db.owner(t.group)?
    );
    for worker in [t.source.worker, t.target] {
        if let Ok(addr) = db.address(worker) {
            if let Ok(mut peer) = connect(dir, addr, &controller_credential(dir, t)) {
                let _ = peer.call(&Request::Control(t, zz_mesh::remote::Command::Abort));
            }
        }
    }
    forget(&control, t)
}
fn controller_credential(dir: &Path, t: Handoff) -> String {
    if dir.join("operator.key").is_file() {
        "operator".into()
    } else {
        worker_name(t.source.worker)
    }
}
fn status(dir: &Path, addr: SocketAddr) -> Result<()> {
    let db = Store::open(dir.join("world.sqlite"))?;
    let credential = if dir.join("operator.key").is_file() {
        "operator".into()
    } else {
        worker_name(db.worker(1)?)
    };
    let mut worker = connect(dir, addr, &credential)?;
    let Response::Diagnostics {
        views,
        sent,
        received,
        last_error,
    } = worker.call(&Request::Diagnostics)?
    else {
        return Err(invalid("worker diagnostics unavailable"));
    };
    println!(
        "Group owner {:?}. {} subscribed views, {} frames sent, {} applied.",
        db.owner(GROUP)?,
        views,
        sent,
        received
    );
    if let Some(reason) = last_error {
        println!("Latest diagnostic: {reason}");
    }
    Ok(())
}
fn client(dir: &Path, bootstrap: SocketAddr) -> Result<()> {
    let outbox = Connection::open(dir.join("client.sqlite"))?;
    outbox.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS client_state(id INTEGER PRIMARY KEY,state BLOB NOT NULL)")?;
    let saved: Option<Vec<u8>> = outbox
        .query_row("SELECT state FROM client_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let fresh = saved.is_none();
    let mut prediction = match saved {
        Some(bytes) => decode(&bytes)?,
        None => Prediction::new(Id::new(), GROUP, Inventory::default()),
    };
    let mut entry = connect(dir, bootstrap, "player")?;
    let mut started = false;
    for _ in 0..8 {
        let Response::RouteTo { address, .. } = entry.call(&Request::Locate(GROUP))? else {
            return Err(invalid("no authoritative worker address"));
        };
        let mut wire = connect(dir, address, "player")?;
        if fresh && !started && !refresh(&mut prediction, &mut wire)? {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if !started && prediction.retry().is_empty() {
            prediction.submit(
                Predicted::Drop(DropItems(vec![Stack {
                    kind: STONE,
                    count: 1,
                }])),
                None,
            )?;
        }
        started = true;
        save_prediction(&outbox, &prediction)?;
        if let Ok(Response::Actions(replies)) = wire.call(&Request::Actions(prediction.retry())) {
            for reply in replies {
                match reply {
                    Reply::Decided(r) => prediction.receive(&r)?,
                    Reply::Resync(_) => {
                        refresh(&mut prediction, &mut wire)?;
                    }
                    _ => break,
                }
            }
            save_prediction(&outbox, &prediction)?;
            if prediction.retry().is_empty() {
                println!(
                    "Confirmed sequence {}. Inventory {:?}",
                    prediction.decided,
                    prediction.visible().inventory
                );
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(invalid(
        "authoritative worker unavailable, pending actions saved in client.sqlite",
    ))
}
fn save_prediction(c: &Connection, p: &Prediction) -> Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO client_state VALUES(1,?)",
        [encode(p)?],
    )?;
    Ok(())
}
fn refresh(p: &mut Prediction, client: &mut Client) -> Result<bool> {
    let Response::Confirmed {
        revision,
        sequence,
        actor,
    } = client.call(&Request::Sync {
        session: p.session,
        group: GROUP,
    })?
    else {
        return Ok(false);
    };
    p.resync(
        revision,
        sequence,
        Visible {
            inventory: actor.get(INVENTORY, 1)?,
            ..Default::default()
        },
    )?;
    Ok(true)
}
