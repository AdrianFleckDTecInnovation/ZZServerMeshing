use crate::*;
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use zz_mesh::wire::{Identity, Role, Service};

pub(crate) fn write_new_credential(path: &Path, data: &[u8]) -> mesh::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    f.write_all(data)?;
    f.as_file().sync_all()?;
    f.persist_noclobber(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub(crate) fn issuer_params() -> mesh::Result<CertificateParams> {
    let mut p = CertificateParams::new(vec![]).map_err(|e| invalid(e.to_string()))?;
    p.distinguished_name
        .push(rcgen::DnType::CommonName, "Infinite Stars development CA");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.serial_number = Some(1u64.into());
    p.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    Ok(p)
}
fn issue(
    dir: &Path,
    name: &str,
    usage: ExtendedKeyUsagePurpose,
    ca: &rcgen::Certificate,
    issuer: &KeyPair,
) -> mesh::Result<()> {
    crate::deployment::credential(
        dir,
        name,
        vec!["localhost".into(), "127.0.0.1".into()],
        usage,
        ca,
        issuer,
    )
    .map(|_| ())
}
pub fn init_tls(dir: &Path) -> mesh::Result<()> {
    fs::create_dir_all(dir)?;
    let mut guard = rusqlite::Connection::open(dir.join("provision.sqlite"))?;
    guard.busy_timeout(std::time::Duration::from_secs(5))?;
    let lock = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !dir.join("ca.der").try_exists()? {
        let key_path = dir.join("issuer.pem");
        let issuer = match fs::read_to_string(&key_path) {
            Ok(pem) => KeyPair::from_pem(&pem).map_err(|e| invalid(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = KeyPair::generate().map_err(|e| invalid(e.to_string()))?;
                write_new_credential(&key_path, key.serialize_pem().as_bytes())?;
                key
            }
            Err(e) => return Err(e.into()),
        };
        let ca = issuer_params()?
            .self_signed(&issuer)
            .map_err(|e| invalid(e.to_string()))?;
        issue(
            dir,
            "server",
            ExtendedKeyUsagePurpose::ServerAuth,
            &ca,
            &issuer,
        )?;
        issue(
            dir,
            "player",
            ExtendedKeyUsagePurpose::ClientAuth,
            &ca,
            &issuer,
        )?;
        write_new_credential(&dir.join("ca.der"), ca.der())?;
    }
    lock.commit()?;
    Ok(())
}
pub fn worker_name(w: Worker) -> String {
    format!("worker-{}-{}", w.slot, w.incarnation)
}
pub fn provision_worker(dir: &Path, db: &Store, w: Worker) -> mesh::Result<()> {
    let issuer = KeyPair::from_pem(&fs::read_to_string(dir.join("issuer.pem"))?)
        .map_err(|e| invalid(e.to_string()))?;
    let ca = issuer_params()?
        .self_signed(&issuer)
        .map_err(|e| invalid(e.to_string()))?;
    let name = worker_name(w);
    issue(
        dir,
        &name,
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
        &issuer,
    )?;
    db.bind_identity(w, digest(&fs::read(dir.join(format!("{name}.der")))?))
}
pub fn identity(dir: &Path, name: &str) -> mesh::Result<Identity> {
    Ok(Identity {
        cert: fs::read(dir.join(format!("{name}.der")))?,
        key: fs::read(dir.join(format!("{name}.key")))?,
    })
}
pub fn service(db: &Store, w: Worker) -> Arc<Service> {
    let mut app = world(db, w);
    Arc::new(Service {
        store: db.clone(),
        worker: w,
        actions: app.world_mut().remove_resource().unwrap(),
        providers: app.world_mut().remove_resource().unwrap(),
        replicas: Default::default(),
        bulk: Default::default(),
    })
}
pub fn start_node(db: &Store, slot: u32) -> mesh::Result<(Worker, App)> {
    let worker = db.start_worker(slot)?;
    Ok((worker, resume_node(db, worker)?))
}
pub(crate) fn resume_node(db: &Store, worker: Worker) -> mesh::Result<App> {
    let slot = worker.slot;
    let mut app = world(db, worker);
    for group in db.owned(slot)? {
        mesh::restore_group(app.world_mut(), group)?;
    }
    if slot == 1 && db.group_of(ACTOR)?.is_none() {
        let empty = match db.owner(GROUP) {
            Ok(owner) => owner.worker == worker && db.snapshot(GROUP)?.records.is_empty(),
            Err(mesh::Error::Missing(_)) => true,
            Err(err) => return Err(err),
        };
        if empty {
            seed(&mut app)?;
            checkpoint_group(app.world_mut(), GROUP)?;
            if !matches!(db.lookup(ACTOR)?, Lookup::Present(_)) {
                return Err(invalid("initial player checkpoint missing"));
            }
            // Resolve local Entity references and install the verified blueprint on first boot.
            load_group(&mut app, db, GROUP, worker)?;
        }
    }
    Ok(app)
}
pub fn load_group(app: &mut App, db: &Store, group: Id, w: Worker) -> mesh::Result<()> {
    let owner = db.owner(group)?;
    if owner.worker != w {
        return Err(mesh::Error::Fenced);
    }
    db.snapshot(group)?
        .prepare_with_data(app.world_mut())?
        .activate(app.world_mut(), db, owner)
}
pub fn listen(
    dir: PathBuf,
    slot: u32,
    addr: std::net::SocketAddr,
    ready: Option<PathBuf>,
) -> mesh::Result<()> {
    init_tls(&dir)?;
    let db = Store::open(dir.join("world.sqlite"))?;
    let (w, app) = start_node(&db, slot)?;
    provision_worker(&dir, &db, w)?;
    let cred = identity(&dir, &worker_name(w))?;
    run_node(dir, db, w, app, cred, addr, ready)
}
pub(crate) fn run_node(
    dir: PathBuf,
    db: Store,
    w: Worker,
    mut app: App,
    cred: Identity,
    addr: std::net::SocketAddr,
    ready: Option<PathBuf>,
) -> mesh::Result<()> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Limits {
        tick_budget_us: u64,
        memory_bytes: u64,
    }
    match fs::read(dir.join("capacity.json")) {
        Ok(bytes) => {
            let limits: Limits = serde_json::from_slice(&bytes)
                .map_err(|e| invalid(format!("capacity.json: {e}")))?;
            if limits.tick_budget_us == 0 || limits.memory_bytes == 0 {
                return Err(invalid("capacity budgets must be positive"));
            }
            app.insert_resource(BalancePolicy {
                tick_budget_us: limits.tick_budget_us,
                memory_bytes: limits.memory_bytes,
                ..Default::default()
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let player = digest(&fs::read(dir.join("player.der"))?);
    let distributed = dir.join("enrol.der").exists();
    let operator = if distributed {
        Some(digest(&fs::read(dir.join("operator.der"))?))
    } else {
        None
    };
    let registry = db.clone();
    let listener = zz_mesh::wire::Listener::bind_resolved(
        addr,
        &fs::read(dir.join("ca.der"))?,
        &identity(&dir, "server")?,
        move |fingerprint| {
            if fingerprint == player {
                Ok(Role::Player(ACTOR))
            } else if Some(fingerprint) == operator {
                Ok(Role::Operator)
            } else {
                registry.authenticated_worker(fingerprint).map(Role::Worker)
            }
        },
    )?;
    let address = listener.address()?;
    let advertised = match fs::read_to_string(dir.join("advertise.txt")) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|_| invalid("invalid advertised socket address"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => address,
        Err(e) => return Err(e.into()),
    };
    if distributed && (advertised.ip().is_unspecified() || advertised.port() == 0) {
        return Err(invalid("set advertise.txt to the reachable IP and port"));
    }
    db.advertise(w, advertised)?;
    if let Some(path) = ready {
        fs::write(path, address.to_string())?;
    }
    println!(
        "worker {} incarnation {} listening on {}",
        w.slot, w.incarnation, address
    );
    let (svc, mut event_loop) =
        mesh::remote::worker(service(&db, w), app.world().resource::<BlobCache>().clone());
    let ca = fs::read(dir.join("ca.der"))?;
    let registry = db.clone();
    event_loop.peers = Some(mesh::network_mesh::PeerMesh::new(
        db.clone(),
        w,
        move |target| {
            let name = if distributed {
                crate::deployment::worker_dns(target.slot)
            } else {
                "localhost".into()
            };
            mesh::wire::Client::connect(registry.address(target)?, &name, &ca, &cred)
        },
    ));
    let connections = if distributed { 256 } else { 16 };
    let thread = std::thread::spawn(move || listener.serve(svc, Default::default(), connections));
    let mut next = std::time::Instant::now();
    while !thread.is_finished() {
        if std::time::Instant::now() >= next {
            let tick_start = std::time::Instant::now();
            app.update();
            if let Some(peers) = event_loop.peers.as_mut() {
                if let Err(e) = peers.step(app.world_mut()) {
                    app.world_mut()
                        .resource_mut::<MeshState>()
                        .diagnose(e.to_string());
                }
            }
            event_loop.record_tick(tick_start.elapsed());
            next = tick_start + std::time::Duration::from_millis(50);
        }
        event_loop.pump(app.world_mut(), 16, std::time::Duration::from_millis(3));
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    thread.join().map_err(|_| invalid("listener stopped"))?
}
