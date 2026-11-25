use crate::*;
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use zz_mesh::wire::{Identity, Role, Service};

fn keyfile(path: &Path, data: &[u8]) -> mesh::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}
fn issuer_params() -> mesh::Result<CertificateParams> {
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
    let key = KeyPair::generate().map_err(|e| invalid(e.to_string()))?;
    let mut p = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .map_err(|e| invalid(e.to_string()))?;
    p.distinguished_name.push(rcgen::DnType::CommonName, name);
    p.extended_key_usages = vec![usage];
    let cert = p
        .signed_by(&key, ca, issuer)
        .map_err(|e| invalid(e.to_string()))?;
    keyfile(&dir.join(format!("{name}.key")), &key.serialize_der())?;
    fs::write(dir.join(format!("{name}.der")), cert.der())?;
    Ok(())
}
pub fn init_tls(dir: &Path) -> mesh::Result<()> {
    fs::create_dir_all(dir)?;
    let mut guard = rusqlite::Connection::open(dir.join("provision.sqlite"))?;
    guard.busy_timeout(std::time::Duration::from_secs(5))?;
    let lock = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !dir.join("ca.der").exists() {
        let issuer = KeyPair::generate().map_err(|e| invalid(e.to_string()))?;
        let ca = issuer_params()?
            .self_signed(&issuer)
            .map_err(|e| invalid(e.to_string()))?;
        keyfile(&dir.join("issuer.pem"), issuer.serialize_pem().as_bytes())?;
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
        fs::write(dir.join("ca.der"), ca.der())?;
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
    let mut app = world(db, worker);
    for group in db.owned(slot)? {
        mesh::restore_group(app.world_mut(), group)?;
    }
    if slot == 1 && matches!(db.lookup(ACTOR)?, Lookup::Unknown) {
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
    Ok((worker, app))
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
    let (w, mut app) = start_node(&db, slot)?;
    provision_worker(&dir, &db, w)?;
    let player = digest(&fs::read(dir.join("player.der"))?);
    let registry = db.clone();
    let listener = zz_mesh::wire::Listener::bind_resolved(
        addr,
        &fs::read(dir.join("ca.der"))?,
        &identity(&dir, "server")?,
        move |fingerprint| {
            if fingerprint == player {
                Ok(Role::Player(ACTOR))
            } else {
                registry.authenticated_worker(fingerprint).map(Role::Worker)
            }
        },
    )?;
    let address = listener.address()?;
    db.advertise(w, address)?;
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
    let cred = identity(&dir, &worker_name(w))?;
    let registry = db.clone();
    event_loop.peers = Some(mesh::network_mesh::PeerMesh::new(
        db.clone(),
        w,
        move |target| {
            mesh::wire::Client::connect(registry.address(target)?, "localhost", &ca, &cred)
        },
    ));
    let thread = std::thread::spawn(move || listener.serve(svc, Default::default(), 16));
    let mut next = std::time::Instant::now();
    while !thread.is_finished() {
        if std::time::Instant::now() >= next {
            app.update();
            if let Some(peers) = event_loop.peers.as_mut() {
                if let Err(e) = peers.step(app.world_mut()) {
                    app.world_mut()
                        .resource_mut::<MeshState>()
                        .diagnose(e.to_string());
                }
            }
            next = std::time::Instant::now() + std::time::Duration::from_millis(50);
        }
        event_loop.pump(app.world_mut(), 16, std::time::Duration::from_millis(3));
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    thread.join().map_err(|_| invalid("listener stopped"))?
}
