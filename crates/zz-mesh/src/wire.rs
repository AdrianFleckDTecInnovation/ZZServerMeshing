use crate::*;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::WebPkiClientVerifier,
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const MAGIC: [u8; 4] = *b"ZZM1";
pub const WIRE_VERSION: u16 = 1;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Role {
    Player(Id),
    Worker(Worker),
    /// Restricted to signing a fresh worker credential for this provisioned slot.
    Enrolment(u32, Digest),
    Operator,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Spatial(crate::network_mesh::Report),
    Locate(Id),
    Sync {
        session: Id,
        group: Id,
    },
    Diagnostics,
    Input(Input),
    Control(Handoff, crate::remote::Command),
    Actions(Vec<Intent>),
    Approval {
        group: Id,
        intent: Intent,
    },
    Inspect(Id),
    Bulk {
        manifest: Manifest,
        offset: u64,
        limit: u32,
    },
    Push(Vec<Update>),
    Storage(crate::authority_store::Operation),
    Join {
        public_key: Vec<u8>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Routes(crate::network_mesh::Routes),
    RouteTo {
        worker: Worker,
        address: SocketAddr,
    },
    Confirmed {
        revision: u64,
        sequence: u64,
        actor: Record,
    },
    Diagnostics {
        views: usize,
        sent: u64,
        received: u64,
        last_error: Option<String>,
    },
    Input(InputAck),
    Control(crate::remote::Answer),
    Actions(Vec<Reply>),
    Approved,
    Owner(Authority),
    Block {
        offset: u64,
        bytes: Vec<u8>,
    },
    Applied(Vec<(Id, u64, u64)>),
    Error(String),
    Stored(std::result::Result<Vec<u8>, crate::authority_store::Failure>),
    Joined {
        worker: Worker,
        certificate: Vec<u8>,
    },
}
pub trait Endpoint: Send + Sync + 'static {
    fn dispatch(&self, role: Role, request: Request) -> Result<Response>;
}
pub struct Service {
    pub store: Store,
    pub worker: Worker,
    pub actions: Actions,
    pub providers: Providers,
    pub replicas: std::sync::Mutex<Views>,
    pub bulk: std::sync::Mutex<BlobCache>,
}
impl Endpoint for Service {
    fn dispatch(&self, role: Role, r: Request) -> Result<Response> {
        match r {
            Request::Storage(_) | Request::Join { .. } => Err(Error::Denied),
            Request::Sync { .. }
            | Request::Diagnostics
            | Request::Spatial(_)
            | Request::Input(_)
            | Request::Control(..) => Err(invalid("worker event loop required")),
            Request::Actions(batch) => {
                let Role::Player(actor) = role else {
                    return Err(Error::Denied);
                };
                if batch.len() > MAX_BATCH || batch.iter().any(|i| i.bytes.len() > MAX_FIELD) {
                    return Err(Error::Busy("action batch"));
                }
                let Some(first) = batch.first() else {
                    return Ok(Response::Actions(vec![]));
                };
                if batch
                    .iter()
                    .any(|i| i.group != first.group || i.key.session != first.key.session)
                {
                    return Err(invalid("batch requires one primary group and session"));
                }
                let a = self.store.owner(first.group)?;
                if a.worker != self.worker {
                    return Err(Error::Fenced);
                }
                self.store.session(first.key.session, actor)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| invalid("server clock"))?
                    .as_millis() as u64;
                Ok(Response::Actions(self.store.execute(
                    &self.actions,
                    actor,
                    a,
                    now,
                    &batch,
                )?))
            }
            Request::Locate(group) => {
                let Role::Player(actor) = role else {
                    return Err(Error::Denied);
                };
                if self.store.group_of(actor)? != Some(group) {
                    return Err(Error::Denied);
                }
                let worker = self.store.owner(group)?.worker;
                Ok(Response::RouteTo {
                    worker,
                    address: self.store.address(worker)?,
                })
            }
            Request::Inspect(g) => {
                let Role::Worker(caller) = role else {
                    return Err(Error::Denied);
                };
                self.store.check_worker(caller)?;
                Ok(Response::Owner(self.store.owner(g)?))
            }
            Request::Approval { group, intent } => {
                let Role::Worker(caller) = role else {
                    return Err(Error::Denied);
                };
                let owner = self.store.owner(group)?;
                if caller != owner.worker || owner.worker != self.worker {
                    return Err(Error::Denied);
                }
                self.store.approve(group, owner, &intent)?;
                Ok(Response::Approved)
            }
            Request::Bulk {
                manifest,
                offset,
                limit,
            } => {
                match role {
                    Role::Worker(caller) => self.store.check_worker(caller)?,
                    Role::Operator => (),
                    _ => return Err(Error::Denied),
                }
                if limit == 0 || limit as usize > BLOCK || offset > manifest.length {
                    return Err(invalid("invalid bulk range"));
                }
                let p = self.providers.get(manifest.key.provider)?;
                let mut cache = self.bulk.lock().map_err(|_| invalid("bulk source cache"))?;
                if !cache.ready(&manifest) {
                    cache.fetch(&manifest, p.as_ref())?;
                }
                let artifact = cache
                    .get(&manifest)
                    .ok_or_else(|| Error::Missing("source artifact".into()))?;
                drop(cache);
                let start = offset as usize;
                let bytes = artifact[start..(start + limit as usize).min(artifact.len())].to_vec();
                Ok(Response::Block { offset, bytes })
            }
            Request::Push(updates) => {
                let Role::Worker(caller) = role else {
                    return Err(Error::Denied);
                };
                if updates.len() > MAX_TARGETS {
                    return Err(Error::Busy("state batch"));
                }
                let mut views = self.replicas.lock().map_err(|_| invalid("view mutex"))?;
                let mut acknowledgements = vec![];
                for u in updates {
                    let group = self
                        .store
                        .group_of(u.object)?
                        .ok_or_else(|| Error::Missing(u.object.to_string()))?;
                    let a = self.store.owner(group)?;
                    if a.worker != caller || a.generation != u.generation {
                        return Err(Error::Fenced);
                    }
                    if views.apply(&u) {
                        acknowledgements.push((u.object, u.epoch, u.serial));
                    }
                }
                Ok(Response::Applied(acknowledgements))
            }
        }
    }
}
/// The in-process adapter transfers ownership of the typed message directly.
pub fn direct(service: &dyn Endpoint, role: Role, req: Request) -> Result<Response> {
    service.dispatch(role, req)
}
#[derive(Default)]
pub struct Traffic {
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub payload_out: AtomicU64,
    pub payload_in: AtomicU64,
    pub encode_ns: AtomicU64,
    pub decode_ns: AtomicU64,
    pub encode_cycles: AtomicU64,
    pub decode_cycles: AtomicU64,
}
struct Counted(TcpStream, Arc<Traffic>);
impl Read for Counted {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(b)?;
        self.1.received.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}
impl Write for Counted {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.0.write(b)?;
        self.1.sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
fn framed<T: Serialize>(w: &mut impl Write, value: &T, stats: &Traffic) -> Result<()> {
    use bincode::Options;
    let now = thread_cpu_ns();
    let cycles = thread_cpu_cycles();
    let mut frame = Vec::with_capacity(256);
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    frame.extend_from_slice(&[0; 4]);
    bincode::DefaultOptions::new()
        .with_little_endian()
        .with_varint_encoding()
        .with_limit(MAX_FRAME as u64)
        .serialize_into(&mut frame, value)?;
    let n = (frame.len() - 10) as u32;
    frame[6..10].copy_from_slice(&n.to_le_bytes());
    stats
        .encode_ns
        .fetch_add(thread_cpu_ns().saturating_sub(now), Ordering::Relaxed);
    stats.encode_cycles.fetch_add(
        thread_cpu_cycles().saturating_sub(cycles),
        Ordering::Relaxed,
    );
    w.write_all(&frame)?;
    w.flush()?;
    stats.payload_out.fetch_add(u64::from(n), Ordering::Relaxed);
    Ok(())
}
fn receive<T: DeserializeOwned>(r: &mut impl Read, stats: &Traffic) -> Result<T> {
    let mut h = [0; 10];
    r.read_exact(&mut h)?;
    if h[..4] != MAGIC || u16::from_le_bytes([h[4], h[5]]) != WIRE_VERSION {
        return Err(invalid("unsupported protocol version"));
    }
    let n = u32::from_le_bytes(h[6..].try_into().unwrap()) as usize;
    if n > MAX_FRAME {
        return Err(Error::Busy("frame length"));
    }
    let mut bytes = vec![0; n];
    r.read_exact(&mut bytes)?;
    stats.payload_in.fetch_add(n as u64, Ordering::Relaxed);
    let now = thread_cpu_ns();
    let cycles = thread_cpu_cycles();
    let v = decode(&bytes)?;
    stats.decode_cycles.fetch_add(
        thread_cpu_cycles().saturating_sub(cycles),
        Ordering::Relaxed,
    );
    stats
        .decode_ns
        .fetch_add(thread_cpu_ns().saturating_sub(now), Ordering::Relaxed);
    Ok(v)
}
#[derive(Clone)]
pub struct Identity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}
impl Identity {
    fn parts(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from(self.cert.clone())],
            PrivatePkcs8KeyDer::from(self.key.clone()).into(),
        )
    }
}
fn roots(ca: &[u8]) -> Result<RootCertStore> {
    let mut r = RootCertStore::empty();
    r.add(CertificateDer::from(ca))
        .map_err(|e| invalid(e.to_string()))?;
    Ok(r)
}
fn timeout(socket: &TcpStream) -> Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(())
}
pub struct Client {
    stream: StreamOwned<ClientConnection, Counted>,
    pub traffic: Arc<Traffic>,
}
impl Client {
    pub fn connect(addr: SocketAddr, name: &str, ca: &[u8], id: &Identity) -> Result<Self> {
        Self::connect_counted(addr, name, ca, id, Arc::default())
    }
    pub(crate) fn connect_counted(
        addr: SocketAddr,
        name: &str,
        ca: &[u8],
        id: &Identity,
        traffic: Arc<Traffic>,
    ) -> Result<Self> {
        let (cert, key) = id.parts();
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots(ca)?)
            .with_client_auth_cert(cert, key)
            .map_err(|e| invalid(e.to_string()))?;
        let client = ClientConnection::new(
            Arc::new(cfg),
            ServerName::try_from(name.to_owned()).map_err(|_| invalid("TLS name"))?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(4))?;
        timeout(&tcp)?;
        Ok(Self {
            stream: StreamOwned::new(client, Counted(tcp, traffic.clone())),
            traffic,
        })
    }
    pub fn call(&mut self, r: &Request) -> Result<Response> {
        framed(&mut self.stream, r, &self.traffic)?;
        receive(&mut self.stream, &self.traffic)
    }
}
pub struct Listener {
    tcp: TcpListener,
    tls: Arc<ServerConfig>,
    peers: Arc<dyn Fn(Digest) -> Result<Role> + Send + Sync>,
    pub traffic: Arc<Traffic>,
}
impl Listener {
    pub fn bind(
        addr: SocketAddr,
        ca: &[u8],
        id: &Identity,
        peers: BTreeMap<Digest, Role>,
    ) -> Result<Self> {
        Self::bind_resolved(addr, ca, id, move |hash| {
            peers.get(&hash).copied().ok_or(Error::Denied)
        })
    }
    pub fn bind_resolved(
        addr: SocketAddr,
        ca: &[u8],
        id: &Identity,
        resolve: impl Fn(Digest) -> Result<Role> + Send + Sync + 'static,
    ) -> Result<Self> {
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots(ca)?))
            .build()
            .map_err(|e| invalid(e.to_string()))?;
        let (chain, key) = id.parts();
        let tls = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(|e| invalid(e.to_string()))?;
        let tcp = TcpListener::bind(addr)?;
        tcp.set_nonblocking(true)?;
        Ok(Self {
            tcp,
            tls: Arc::new(tls),
            peers: Arc::new(resolve),
            traffic: Arc::default(),
        })
    }
    pub fn address(&self) -> Result<SocketAddr> {
        Ok(self.tcp.local_addr()?)
    }
    pub fn serve(
        self,
        service: Arc<dyn Endpoint>,
        stop: Arc<AtomicBool>,
        max_connections: usize,
    ) -> Result<()> {
        let active = Arc::new(AtomicUsize::new(0));
        while !stop.load(Ordering::Acquire) {
            let tcp = match self.tcp.accept() {
                Ok((tcp, _)) => tcp,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if active.load(Ordering::Acquire) >= max_connections {
                drop(tcp);
                continue;
            }
            timeout(&tcp)?;
            let (tls, peers, stats, svc, done) = (
                self.tls.clone(),
                self.peers.clone(),
                self.traffic.clone(),
                service.clone(),
                active.clone(),
            );
            active.fetch_add(1, Ordering::AcqRel);
            std::thread::spawn(move || {
                let run = || -> Result<()> {
                    let mut conn =
                        ServerConnection::new(tls).map_err(|e| invalid(e.to_string()))?;
                    let mut socket = Counted(tcp, stats.clone());
                    while conn.is_handshaking() {
                        conn.complete_io(&mut socket)?;
                    }
                    let cert = conn
                        .peer_certificates()
                        .and_then(|c| c.first())
                        .ok_or(Error::Denied)?;
                    let role = peers(digest(cert.as_ref()))?;
                    let mut stream = StreamOwned::new(conn, socket);
                    for _ in 0..4096 {
                        let req = receive(&mut stream, &stats)?;
                        let answer = svc
                            .dispatch(role, req)
                            .unwrap_or_else(|e| Response::Error(e.to_string()));
                        framed(&mut stream, &answer, &stats)?;
                    }
                    Ok(())
                };
                if let Err(e) = run() {
                    if !matches!(&e,Error::Io(err) if matches!(err.kind(),std::io::ErrorKind::UnexpectedEof|std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::TimedOut|std::io::ErrorKind::WouldBlock))
                    {
                        eprintln!("mesh connection closed: {e}");
                    }
                }
                done.fetch_sub(1, Ordering::AcqRel);
            });
        }
        Ok(())
    }
}
