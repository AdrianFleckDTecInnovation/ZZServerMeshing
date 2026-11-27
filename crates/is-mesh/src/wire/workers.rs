//! Prepared session worker. Idle TLS sockets share one I/O loop. Only complete
//! requests and certificate-to-role lookups consume a bounded blocking job.
use super::*;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;

const DEADLINE: Duration = Duration::from_secs(5);
type Resolver = dyn Fn(Digest) -> Result<Role> + Send + Sync;

struct CountedSocket(tokio::net::TcpStream, Arc<Traffic>);
impl AsyncRead for CountedSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.0).poll_read(cx, buf);
        self.1
            .received
            .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        result
    }
}
impl AsyncWrite for CountedSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.0).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result {
            self.1.sent.fetch_add(*n as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

struct Server {
    tls: TlsAcceptor,
    peers: Arc<Resolver>,
    traffic: Arc<Traffic>,
    service: Arc<dyn Endpoint>,
    jobs: Arc<Semaphore>,
    stop: Arc<AtomicBool>,
}
impl Server {
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let slot = self
            .jobs
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Fenced)?;
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            f()
        })
        .await
        .map_err(|_| Error::Fenced)?
    }
    async fn peer(&self, tcp: tokio::net::TcpStream) -> Result<()> {
        tcp.set_nodelay(true)?;
        let mut tls = tokio::time::timeout(
            DEADLINE,
            self.tls.accept(CountedSocket(tcp, self.traffic.clone())),
        )
        .await
        .map_err(|_| Error::Busy("mesh handshake timeout"))??;
        let cert = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .ok_or(Error::Denied)?;
        let key = digest(cert.as_ref());
        let resolve = self.peers.clone();
        let role = self.blocking(move || resolve(key)).await?;
        for _ in 0..4096 {
            let bytes = tokio::time::timeout(DEADLINE, async {
                let mut header = [0; 10];
                tls.read_exact(&mut header).await?;
                let mut bytes = vec![0; frame_length(&header)?];
                tls.read_exact(&mut bytes).await?;
                Ok::<_, Error>(bytes)
            })
            .await
            .map_err(|_| Error::Busy("mesh frame timeout"))??;
            let svc = self.service.clone();
            let stats = self.traffic.clone();
            let (reply, retire) = self
                .blocking(move || {
                    let req = decode_payload(&bytes, &stats)?;
                    let size = bytes.len() as u64;
                    drop(bytes);
                    stats.in_service.fetch_add(size, Ordering::Relaxed);
                    let answer = svc
                        .dispatch(role, req)
                        .unwrap_or_else(|e| Response::Error(e.to_string()));
                    stats.in_service.fetch_sub(size, Ordering::Relaxed);
                    let retire = role == Role::Operator && matches!(answer, Response::Stopped);
                    let mut out = Vec::new();
                    framed(&mut out, &answer, &stats)?;
                    Ok((out, retire))
                })
                .await?;
            let written = tokio::time::timeout(DEADLINE, async {
                tls.write_all(&reply).await?;
                tls.flush().await
            })
            .await;
            if retire {
                self.stop.store(true, Ordering::Release);
            }
            written.map_err(|_| Error::Busy("mesh send timeout"))??;
            if retire {
                break;
            }
        }
        Ok(())
    }
}

impl Listener {
    /// Own exactly one I/O loop for this process's session-worker listener.
    /// Capacity bounds all connection tasks. Dispatch parallelism is independent
    /// of the number of idle sockets. The existing mesh wire format is shared.
    pub fn serve_shared(
        self,
        service: Arc<dyn Endpoint>,
        stop: Arc<AtomicBool>,
        capacity: usize,
        jobs: usize,
    ) -> Result<()> {
        if !(1..=16_384).contains(&capacity) || !(1..=capacity.min(64)).contains(&jobs) {
            return Err(invalid("invalid mesh listener budgets"));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(jobs)
            .thread_name("mesh-request")
            .build()?;
        let result = rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(self.tcp)?;
            let server = Arc::new(Server {
                tls: TlsAcceptor::from(self.tls),
                peers: self.peers,
                traffic: self.traffic,
                service,
                jobs: Arc::new(Semaphore::new(jobs)),
                stop: stop.clone(),
            });
            let mut peers = JoinSet::new();
            while !stop.load(Ordering::Acquire) {
                while peers.try_join_next().is_some() {}
                let Ok(next) =
                    tokio::time::timeout(Duration::from_millis(10), listener.accept()).await
                else {
                    continue;
                };
                let (tcp, _) = next?;
                if peers.len() < capacity {
                    let server = server.clone();
                    peers.spawn(async move { server.peer(tcp).await });
                }
            }
            peers.shutdown().await;
            Ok(())
        });
        rt.shutdown_timeout(DEADLINE);
        result
    }
}
