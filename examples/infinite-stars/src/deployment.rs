//! One authority volume and separately provisioned worker and player directories.
use crate::{
    host::{
        identity, init_tls, issuer_params, resume_node_with_capacity, run_node,
        write_new_credential,
    },
    *,
};
use rcgen::{Certificate, CertificateParams, ExtendedKeyUsagePurpose, KeyPair, PublicKeyData};
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use zz_mesh::{
    authority_store::AuthorityService,
    wire::{Client, Endpoint, Identity, Listener, Request, Response, Role},
};

const STORE_DNS: &str = "authority.mesh.internal";
pub const GAME_DNS: &str = "game.mesh.internal";
pub fn worker_dns(slot: u32) -> String {
    format!("worker-{slot}.mesh.internal")
}

fn issuer(dir: &Path) -> mesh::Result<(Certificate, KeyPair)> {
    let key = KeyPair::from_pem(&fs::read_to_string(dir.join("issuer.pem"))?)
        .map_err(|e| invalid(e.to_string()))?;
    let cert = issuer_params()?
        .self_signed(&key)
        .map_err(|e| invalid(e.to_string()))?;
    Ok((cert, key))
}
fn sign(
    subject: &impl PublicKeyData,
    name: &str,
    names: Vec<String>,
    usage: ExtendedKeyUsagePurpose,
    ca: &Certificate,
    issuer: &KeyPair,
) -> mesh::Result<Vec<u8>> {
    let mut p = CertificateParams::new(names).map_err(|e| invalid(e.to_string()))?;
    p.distinguished_name.push(rcgen::DnType::CommonName, name);
    p.extended_key_usages = vec![usage];
    Ok(p.signed_by(subject, ca, issuer)
        .map_err(|e| invalid(e.to_string()))?
        .der()
        .to_vec())
}
pub(crate) fn credential(
    dir: &Path,
    name: &str,
    names: Vec<String>,
    usage: ExtendedKeyUsagePurpose,
    ca: &Certificate,
    signer: &KeyPair,
) -> mesh::Result<Vec<u8>> {
    let key_path = dir.join(format!("{name}.key"));
    let cert_path = dir.join(format!("{name}.der"));
    if cert_path.try_exists()? {
        if !key_path.try_exists()? {
            return Err(invalid(format!(
                "missing private key for {name} certificate"
            )));
        }
        return Ok(fs::read(cert_path)?);
    }
    let key = if key_path.try_exists()? {
        let bytes = fs::read(&key_path)?;
        KeyPair::try_from(bytes.as_slice()).map_err(|e| invalid(e.to_string()))?
    } else {
        let key = KeyPair::generate().map_err(|e| invalid(e.to_string()))?;
        write_new_credential(&key_path, &key.serialize_der())?;
        key
    };
    let cert = sign(&key, name, names, usage, ca, signer)?;
    write_new_credential(&cert_path, &cert)?;
    Ok(cert)
}
fn registry(dir: &Path) -> mesh::Result<Connection> {
    let c = Connection::open(dir.join("provision.sqlite"))?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS enrolments(slot INTEGER PRIMARY KEY, fingerprint BLOB UNIQUE NOT NULL)")?;
    Ok(c)
}
fn authority_keys(dir: &Path) -> mesh::Result<()> {
    init_tls(dir)?;
    let mut c = registry(dir)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (ca, signer) = issuer(dir)?;
    credential(
        dir,
        "store",
        vec![STORE_DNS.into()],
        ExtendedKeyUsagePurpose::ServerAuth,
        &ca,
        &signer,
    )?;
    credential(
        dir,
        "operator",
        vec![],
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
        &signer,
    )?;
    tx.commit()?;
    Ok(())
}
/// Export only the material for one trust domain into a new directory.
pub fn provision(dir: &Path, dest: &Path, slot: Option<u32>) -> mesh::Result<()> {
    authority_keys(dir)?;
    if dest.exists() {
        return Err(invalid("credential destination must be a new directory"));
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = tempfile::tempdir_in(parent)?;
    for name in ["ca.der", "player.der"] {
        fs::copy(dir.join(name), temp.path().join(name))?;
    }
    let mut c = registry(dir)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(slot) = slot {
        if !(1..=64).contains(&slot) {
            return Err(invalid("worker slot must be in 1..=64"));
        }
        let (ca, signer) = issuer(dir)?;
        fs::copy(dir.join("operator.der"), temp.path().join("operator.der"))?;
        if dir.join("space.json").try_exists()? {
            SpaceSettings::load(dir)?;
            fs::copy(dir.join("space.json"), temp.path().join("space.json"))?;
        }
        credential(
            temp.path(),
            "server",
            vec![GAME_DNS.into(), worker_dns(slot)],
            ExtendedKeyUsagePurpose::ServerAuth,
            &ca,
            &signer,
        )?;
        let cert = credential(
            temp.path(),
            "enrol",
            vec![],
            ExtendedKeyUsagePurpose::ClientAuth,
            &ca,
            &signer,
        )?;
        tx.execute("INSERT INTO enrolments(slot,fingerprint) VALUES(?,?) ON CONFLICT(slot) DO UPDATE SET fingerprint=excluded.fingerprint",
            rusqlite::params![slot, digest(&cert).0.as_slice()])?;
    } else {
        write_new_credential(
            &temp.path().join("player.key"),
            &fs::read(dir.join("player.key"))?,
        )?;
        fs::write(temp.path().join("server-name"), GAME_DNS)?;
    }
    publish_directory(temp.path(), dest)?;
    tx.commit()?;
    Ok(())
}
fn publish_directory(source: &Path, dest: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        if dest.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                dest.display().to_string(),
            ));
        }
        match fs::rename(source, dest) {
            // Windows can temporarily deny the rename while a handle to a staged
            // credential is closing. Keep the generated keys and the transaction.
            Err(e)
                if cfg!(windows)
                    && matches!(e.raw_os_error(), Some(5 | 32 | 33))
                    && attempts < 6 =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10 << attempts));
                attempts += 1;
            }
            result => return result,
        }
    }
}
/// Issue an independent player identity for a character created by the game.
pub fn provision_player(dir: &Path, dest: &Path, actor: Id) -> mesh::Result<()> {
    authority_keys(dir)?;
    if dest.try_exists()? {
        return Err(invalid("credential destination must be a new directory"));
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = tempfile::tempdir_in(parent)?;
    let (ca, signer) = issuer(dir)?;
    let cert = credential(
        temp.path(),
        "player",
        vec![],
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
        &signer,
    )?;
    fs::copy(dir.join("ca.der"), temp.path().join("ca.der"))?;
    fs::write(temp.path().join("server-name"), GAME_DNS)?;
    let db = Store::open(dir.join("world.sqlite"))?;
    db.register_player(digest(&cert), actor)?;
    publish_directory(temp.path(), dest)?;
    Ok(())
}

struct PublicPoint(Vec<u8>);
impl PublicKeyData for PublicPoint {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }
    fn algorithm(&self) -> &rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}
struct AuthorityHost {
    db: Store,
    service: AuthorityService,
    ca: Certificate,
    signer: KeyPair,
    enrolments: Arc<Mutex<Connection>>,
}
impl Endpoint for AuthorityHost {
    fn dispatch(&self, role: Role, req: Request) -> mesh::Result<Response> {
        if let (Role::Enrolment(slot, fingerprint), Request::Join { public_key }) = (role, &req) {
            if public_key.len() != 65 || public_key[0] != 4 {
                return Err(invalid("expected uncompressed P-256 public key"));
            }
            // Revalidate the provisioning pin even on sockets opened before credential rotation.
            let mut c = self
                .enrolments
                .lock()
                .map_err(|_| invalid("enrolment lock"))?;
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM enrolments WHERE slot=? AND fingerprint=?)",
                rusqlite::params![slot, fingerprint.0.as_slice()],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(mesh::Error::Denied);
            }
            let cert = sign(
                &PublicPoint(public_key.clone()),
                "worker incarnation",
                vec![],
                ExtendedKeyUsagePurpose::ClientAuth,
                &self.ca,
                &self.signer,
            )?;
            let worker = self.db.start_authenticated(slot, digest(&cert))?;
            tx.commit()?;
            return Ok(Response::Joined {
                worker,
                certificate: cert,
            });
        }
        self.service.dispatch(role, req)
    }
}
pub fn serve(dir: &Path, addr: SocketAddr, ready: Option<PathBuf>) -> mesh::Result<()> {
    authority_keys(dir)?;
    let db = Store::open(dir.join("world.sqlite"))?;
    let mut app = App::new();
    app.add_plugins((MeshPlugin::default(), WorldPlugin));
    let actions = app.world_mut().remove_resource::<Actions>().unwrap();
    let (ca, signer) = issuer(dir)?;
    let enrolments = Arc::new(Mutex::new(registry(dir)?));
    let check = enrolments.clone();
    let trusted = db.clone();
    let listener = Listener::bind_resolved(
        addr,
        &fs::read(dir.join("ca.der"))?,
        &identity(dir, "store")?,
        move |fingerprint| {
            let c = check.lock().map_err(|_| invalid("enrolment lock"))?;
            let slot = c
                .query_row(
                    "SELECT slot FROM enrolments WHERE fingerprint=?",
                    [fingerprint.0.as_slice()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(slot) = slot {
                return Ok(Role::Enrolment(slot, fingerprint));
            }
            drop(c);
            trusted.authenticated_worker(fingerprint).map(Role::Worker)
        },
    )?;
    let address = listener.address()?;
    let service = AuthorityService::new(db.clone(), actions)?;
    println!("Authority listening on {address}");
    if let Some(file) = ready {
        fs::write(file, address.to_string())?;
    }
    listener.serve(
        Arc::new(AuthorityHost {
            db,
            service,
            ca,
            signer,
            enrolments,
        }),
        Default::default(),
        256,
    )
}
pub fn worker(
    dir: PathBuf,
    store_addr: SocketAddr,
    addr: SocketAddr,
    ready: Option<PathBuf>,
) -> mesh::Result<()> {
    let settings = SpaceSettings::load(&dir)?;
    let limits = crate::host::HostLimits::read(&dir)?;
    let (db, worker, cred) = connect_worker(&dir, store_addr)?;
    let app = resume_node_with_capacity(&db, worker, settings, limits.session_capacity)?;
    run_node(dir, db, worker, app, cred, addr, ready, limits)
}
/// Redeem the per-slot credential for a process-specific key and a fenced store.
pub fn connect_worker(
    dir: &Path,
    store_addr: SocketAddr,
) -> mesh::Result<(Store, Worker, Identity)> {
    let ca = fs::read(dir.join("ca.der"))?;
    let mut join = Client::connect(store_addr, STORE_DNS, &ca, &identity(dir, "enrol")?)?;
    let key = KeyPair::generate().map_err(|e| invalid(e.to_string()))?;
    let Response::Joined {
        worker,
        certificate,
    } = join.call(&Request::Join {
        public_key: key.public_key_raw().to_vec(),
    })?
    else {
        return Err(invalid("authority refused worker enrolment"));
    };
    drop(join);
    let cred = Identity {
        cert: certificate,
        key: key.serialize_der(),
    };
    let db = Store::connect(store_addr, STORE_DNS.into(), ca, cred.clone());
    Ok((db, worker, cred))
}

#[cfg(test)]
mod provisioning_tests {
    use super::*;

    #[test]
    fn interrupted_first_boot_reuses_the_issuer_and_published_credentials() {
        let original = tempfile::tempdir().unwrap();
        init_tls(original.path()).unwrap();
        let interrupted = tempfile::tempdir().unwrap();
        for name in ["issuer.pem", "server.key", "server.der", "player.key"] {
            fs::copy(original.path().join(name), interrupted.path().join(name)).unwrap();
        }
        init_tls(interrupted.path()).unwrap();
        for name in ["issuer.pem", "server.key", "server.der", "player.key"] {
            assert!(
                fs::read(original.path().join(name)).unwrap()
                    == fs::read(interrupted.path().join(name)).unwrap()
            );
        }
        let cert = fs::read(interrupted.path().join("player.der")).unwrap();
        assert!(!cert.is_empty());
        init_tls(interrupted.path()).unwrap();
        assert_eq!(
            fs::read(interrupted.path().join("player.der")).unwrap(),
            cert
        );
    }

    #[test]
    fn staging_publishes_the_complete_directory_and_preserves_an_existing_export() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("staging");
        let dest = root.path().join("published");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("marker"), b"complete").unwrap();
        #[cfg(windows)]
        let holder = {
            use std::os::windows::fs::OpenOptionsExt;
            let dir = fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .custom_flags(0x0200_0000)
                .open(&stage)
                .unwrap(); // FILE_FLAG_BACKUP_SEMANTICS
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(40));
                drop(dir);
            })
        };
        publish_directory(&stage, &dest).unwrap();
        #[cfg(windows)]
        holder.join().unwrap();
        assert_eq!(fs::read(dest.join("marker")).unwrap(), b"complete");
        fs::create_dir(&stage).unwrap();
        assert_eq!(
            publish_directory(&stage, &dest).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(stage.is_dir());
    }
}
