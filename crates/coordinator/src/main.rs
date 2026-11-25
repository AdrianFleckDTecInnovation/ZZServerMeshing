mod registry;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use registry::Registry;
use shared::protocol::CoordClientMsg;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

#[derive(Parser, Debug)]
#[command(name = "coordinator", about = "Server mesh coordinator")]
struct Args {
    /// Port for sim-server connections (WebSocket)
    #[arg(long, default_value_t = 7870)]
    port: u16,

    /// HTTP port for client assignment endpoint
    #[arg(long, default_value_t = 7871)]
    http_port: u16,

    /// Directory to serve static client files from (e.g., built Vite output).
    /// When set, serves files on all non-API paths instead of the coordinator dashboard.
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Path to TLS certificate file (PEM). When both --tls-cert and --tls-key
    /// are provided, the HTTP server serves HTTPS instead of plain HTTP.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// Path to TLS private key file (PEM).
    #[arg(long)]
    tls_key: Option<PathBuf>,
}

/// Internal event from a sim-server connection to the main registry loop.
enum RegistryEvent {
    Register {
        server_id: u16,
        udp_port: u16,
        ws_port: u16,
        host: Option<String>,
        tls: bool,
        /// Channel to send messages back to this server's WS connection.
        reply_tx: mpsc::UnboundedSender<String>,
    },
    Heartbeat {
        server_id: u16,
        entity_count: u32,
        weighted_load: u32,
    },
    Disconnected {
        server_id: u16,
        /// The reply_tx from this connection, used to verify we only unregister
        /// the correct entry (not a newer re-registered connection).
        reply_tx: mpsc::UnboundedSender<String>,
    },
    Draining {
        server_id: u16,
    },
    TransferResult {
        server_id: u16,
        target_server: u16,
        success: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let registry = Arc::new(RwLock::new(Registry::new()));
    let (event_tx, event_rx) = mpsc::unbounded_channel::<RegistryEvent>();

    // Build TLS config if cert+key provided
    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let acceptor = shared::tls::build_tls_acceptor(cert_path, key_path)
                .expect("Failed to build TLS config from provided cert/key");
            tracing::info!(
                "TLS enabled for HTTP server (cert={}, key={})",
                cert_path.display(),
                key_path.display()
            );
            Some(acceptor)
        }
        (Some(_), None) | (None, Some(_)) => {
            panic!("Both --tls-cert and --tls-key must be provided together");
        }
        (None, None) => None,
    };

    // Spawn the registry manager
    tokio::spawn(run_registry(event_rx, Arc::clone(&registry)));

    // Spawn the HTTP(S) assignment server
    tokio::spawn(run_assignment_server(
        args.http_port,
        Arc::clone(&registry),
        args.static_dir,
        tls_acceptor.clone(),
    ));

    // Accept sim-server WebSocket connections.
    // WI-223 (design 016, Batch 6): when a TLS acceptor is configured, we
    // wrap each accepted TCP stream with it before the WS handshake, so the
    // 7870 port serves WSS. Previously it always bound plain WS; that left
    // the coordinator control channel unencrypted even in the UNO Q TLS
    // deployment. Mac-local dev is unchanged — no --tls-cert means no
    // acceptor means plain WS as before.
    let listener = TcpListener::bind(shared::listen_addr(args.port))
        .await
        .expect("Failed to bind coordinator port");
    let ws_scheme = if tls_acceptor.is_some() { "wss" } else { "ws" };
    tracing::info!("Coordinator listening on port {} ({ws_scheme})", args.port);

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Accept error: {e}");
                continue;
            }
        };

        // TCP keepalive — defense-in-depth for finding 076.
        //
        // Without this, a half-open TCP connection (peer power-off or kill -9
        // with no FIN) can linger in the kernel for minutes to hours; the
        // mpsc sink_task won't exit until ws_sink.send returns an error, which
        // only happens when the kernel surfaces the dead connection. With
        // keepalive set, worst-case dead-connection detection is
        //   time + (retries * interval) = 10s + 3*5s = 25s
        // on linux, well within the 15s heartbeat timeout semantics (the
        // heartbeat timeout fires first; keepalive is the backstop when the
        // sink_task never gets a chance to send).
        //
        // socket2 0.5 supports the probe interval on Windows, but leaves
        // the number of probes to the OS. Keep the existing macOS defaults.
        {
            use socket2::{SockRef, TcpKeepalive};
            let ka = TcpKeepalive::new().with_time(std::time::Duration::from_secs(10));
            #[cfg(not(target_os = "macos"))]
            let ka = ka.with_interval(std::time::Duration::from_secs(5));
            #[cfg(not(any(windows, target_os = "macos")))]
            let ka = ka.with_retries(3);
            let sock = SockRef::from(&stream);
            if let Err(e) = sock.set_tcp_keepalive(&ka) {
                tracing::warn!("set_tcp_keepalive failed for {addr}: {e}");
            }
        }

        let event_tx = event_tx.clone();
        if let Some(ref acceptor) = tls_acceptor {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("TLS handshake error from {addr}: {e}");
                        return;
                    }
                };
                let ws = match tokio_tungstenite::accept_async(tls_stream).await {
                    Ok(ws) => ws,
                    Err(e) => {
                        tracing::error!("WS handshake error (TLS) from {addr}: {e}");
                        return;
                    }
                };
                tracing::info!("Sim-server connecting from {addr} (TLS)");
                handle_server_connection(ws, addr, event_tx).await;
            });
        } else {
            tokio::spawn(async move {
                let ws = match tokio_tungstenite::accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(e) => {
                        tracing::error!("WS handshake error from {addr}: {e}");
                        return;
                    }
                };
                tracing::info!("Sim-server connecting from {addr}");
                handle_server_connection(ws, addr, event_tx).await;
            });
        }
    }
}

/// Handle a single sim-server WebSocket connection. Generic over the
/// transport so we can plug in either a plain `TcpStream` or a
/// `tokio_rustls::server::TlsStream<TcpStream>` — WI-223 adds the TLS branch.
async fn handle_server_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    ws: WebSocketStream<S>,
    addr: std::net::SocketAddr,
    event_tx: mpsc::UnboundedSender<RegistryEvent>,
) {
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();

    let mut server_id: Option<u16> = None;

    // Forward outbound messages to WS
    let sink_task = tokio::spawn(async move {
        while let Some(msg) = reply_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read inbound messages.
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<CoordClientMsg>(&text) {
                Ok(CoordClientMsg::Register {
                    server_id: sid,
                    udp_port,
                    ws_port,
                    host,
                    tls,
                    protocol_version,
                }) => {
                    let sid_raw = sid.0;
                    if protocol_version != shared::PROTOCOL_VERSION && protocol_version != 0 {
                        tracing::warn!(
                            server_id = sid_raw,
                            wire = protocol_version,
                            expected = shared::PROTOCOL_VERSION,
                            "Protocol version mismatch from S{sid_raw}"
                        );
                    }
                    // Use advertised host if provided, otherwise use the peer's source IP
                    let resolved_host = host.unwrap_or_else(|| addr.ip().to_string());
                    server_id = Some(sid_raw);
                    let _ = event_tx.send(RegistryEvent::Register {
                        server_id: sid_raw,
                        udp_port,
                        ws_port,
                        host: Some(resolved_host),
                        tls,
                        reply_tx: reply_tx.clone(),
                    });
                }
                Ok(CoordClientMsg::Heartbeat {
                    server_id: sid,
                    entity_count,
                    weighted_load,
                }) => {
                    let _ = event_tx.send(RegistryEvent::Heartbeat {
                        server_id: sid.0,
                        entity_count,
                        weighted_load,
                    });
                }
                Ok(CoordClientMsg::Draining { server_id: sid }) => {
                    let _ = event_tx.send(RegistryEvent::Draining { server_id: sid.0 });
                }
                Ok(CoordClientMsg::TransferResult {
                    entity_id: _,
                    target_server,
                    success,
                }) => {
                    if let Some(sid) = server_id {
                        let _ = event_tx.send(RegistryEvent::TransferResult {
                            server_id: sid,
                            target_server: target_server.0,
                            success,
                        });
                    }
                }
                Ok(CoordClientMsg::PlayerTransfer {
                    account_id,
                    from_server,
                    to_server,
                }) => {
                    // Placeholder: log the transfer request. Full implementation
                    // requires the SQLite persistence layer (not yet built).
                    tracing::info!(
                        account_id,
                        from_server = from_server.0,
                        to_server = to_server.0,
                        "Player transfer request (persistence not yet implemented)"
                    );
                }
                Err(e) => {
                    tracing::warn!("Bad message from {addr}: {e}");
                }
            }
        }
    }

    // Server disconnected
    if let Some(sid) = server_id {
        tracing::info!("Sim-server S{sid} disconnected ({addr})");
        let _ = event_tx.send(RegistryEvent::Disconnected {
            server_id: sid,
            reply_tx: reply_tx.clone(),
        });
    }
    sink_task.abort();
}

async fn run_registry(
    mut event_rx: mpsc::UnboundedReceiver<RegistryEvent>,
    registry: Arc<RwLock<Registry>>,
) {
    let mut rebalance_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let rebalance_cooldown = std::time::Duration::from_secs(5);
    let mut last_rebalance = std::time::Instant::now() - rebalance_cooldown;

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                let mut reg = registry.write().await;
                match event {
                    RegistryEvent::Register { server_id, udp_port, ws_port, host, tls, reply_tx } => {
                        reg.register(server_id, udp_port, ws_port, host, tls, reply_tx);
                        tracing::info!(
                            "Registered S{server_id} (udp:{udp_port} ws:{ws_port}) | {} servers total",
                            reg.server_count()
                        );
                        reg.broadcast_peer_list();
                    }
                    RegistryEvent::Heartbeat { server_id, entity_count, weighted_load } => {
                        reg.heartbeat(server_id, entity_count, weighted_load);
                    }
                    RegistryEvent::Draining { server_id } => {
                        tracing::info!("S{server_id} requesting graceful drain");
                        reg.drain_server(server_id);
                    }
                    RegistryEvent::TransferResult { server_id, target_server, success } => {
                        reg.record_transfer_result(server_id, target_server, success);
                    }
                    RegistryEvent::Disconnected { server_id, reply_tx } => {
                        // Only unregister if this disconnect is from the current connection,
                        // not a stale one that was replaced by a re-registration.
                        if reg.unregister_if_sender_matches(server_id, &reply_tx) {
                            tracing::info!(
                                "Unregistered S{server_id} | {} servers remain",
                                reg.server_count()
                            );
                            reg.broadcast_peer_list();
                            // If this was an unexpected disconnect (crash), trigger recovery
                            if server_id > 0 {
                                tracing::warn!("S{server_id} crashed — broadcasting recovery");
                                reg.broadcast_recovery(server_id);
                            }
                        }
                    }
                }
            }
            _ = rebalance_interval.tick() => {
                let mut reg = registry.write().await;
                // Send keepalive to all servers (prevents read timeout on stable clusters)
                reg.broadcast_keepalive();
                // Check for dead servers
                let dead = reg.detect_dead_servers();
                for sid in dead {
                    tracing::warn!("S{sid} missed heartbeat — marking as dead");
                    reg.unregister(sid);
                    reg.broadcast_peer_list();
                    reg.broadcast_recovery(sid);
                }
                // Rebalance
                if last_rebalance.elapsed() >= rebalance_cooldown && reg.try_rebalance() {
                    last_rebalance = std::time::Instant::now();
                }
            }
        }
    }
}

const COORDINATOR_DASHBOARD: &str = include_str!("../static/dashboard.html");

/// Build an HTTP response with standard headers.
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "200 OK",
        204 => "204 No Content",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        502 => "502 Bad Gateway",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    };
    let mut resp = format!(
        "HTTP/1.1 {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

/// Convenience wrapper for JSON HTTP responses.
fn json_response(status: u16, body: &str) -> Vec<u8> {
    http_response(status, "application/json", body.as_bytes())
}

/// Handle a single HTTP request on a stream that implements AsyncRead + AsyncWrite.
/// This contains all the routing logic, shared between plain TCP and TLS connections.
async fn handle_http_request<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut stream: S,
    registry: Arc<RwLock<Registry>>,
    static_dir: Option<Arc<PathBuf>>,
) {
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);

    // Handle CORS preflight
    if request.starts_with("OPTIONS") {
        let response = "HTTP/1.1 204 No Content\r\n\
            Access-Control-Allow-Origin: *\r\n\
            Access-Control-Allow-Methods: GET, OPTIONS\r\n\
            Access-Control-Allow-Headers: *\r\n\
            Content-Length: 0\r\n\
            Connection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }

    // Parse request path
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    // Extract client host from Host header
    let client_host: Option<String> = request
        .lines()
        .find(|l| l.len() > 5 && l[..5].eq_ignore_ascii_case("host:"))
        .map(|l| {
            let value = l[5..].trim();
            value.split(':').next().unwrap_or(value).to_string()
        });

    match path {
        "/assign" => {
            let r = registry.read().await;
            let assignment = r.get_least_loaded_ws_addr(client_host.as_deref());
            drop(r);

            let resp = match assignment {
                Some(ws_url) => json_response(200, &format!(r#"{{"ws_url":"{}"}}"#, ws_url)),
                None => json_response(503, r#"{"error":"no servers available"}"#),
            };
            let _ = stream.write_all(&resp).await;
        }
        "/servers" => {
            let r = registry.read().await;
            let list = r.get_server_list();
            let ttl_prunes = r.ttl_prunes_total();
            drop(r);

            let body = serde_json::json!({
                "servers": list,
                "ttl_prunes_total": ttl_prunes,
            })
            .to_string();
            let _ = stream.write_all(&json_response(200, &body)).await;
        }
        "/dashboard" => {
            // Always serve coordinator dashboard at /dashboard
            let resp = http_response(200, "text/html", COORDINATOR_DASHBOARD.as_bytes());
            let _ = stream.write_all(&resp).await;
        }
        _ if path.starts_with("/metrics/") => {
            // Proxy metrics from sim-servers so the HTTPS dashboard can fetch them
            // without mixed-content issues.
            let sid_str = &path["/metrics/".len()..];
            if let Ok(sid) = sid_str.parse::<u16>() {
                let r = registry.read().await;
                let server_info = r.get_server_list();
                drop(r);

                let host = server_info
                    .iter()
                    .find(|s| s["server_id"].as_u64() == Some(sid as u64))
                    .and_then(|s| s["host"].as_str())
                    .map(|h| h.to_string());
                let metrics_port = 8100 + sid;

                if let Some(host) = host {
                    match tokio::net::TcpStream::connect(format!("{host}:{metrics_port}")).await {
                        Ok(mut upstream) => {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let req = format!(
                                "GET /metrics HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
                            );
                            let _ = upstream.write_all(req.as_bytes()).await;
                            let mut buf = Vec::new();
                            let _ = upstream.read_to_end(&mut buf).await;
                            let resp = String::from_utf8_lossy(&buf);
                            if let Some(idx) = resp.find("\r\n\r\n") {
                                let body = &resp[idx + 4..];
                                let _ = stream.write_all(&json_response(200, body)).await;
                            } else {
                                let _ = stream
                                    .write_all(&json_response(
                                        502,
                                        r#"{"error":"bad upstream response"}"#,
                                    ))
                                    .await;
                            }
                        }
                        Err(_) => {
                            let _ = stream
                                .write_all(&json_response(
                                    502,
                                    r#"{"error":"upstream unreachable"}"#,
                                ))
                                .await;
                        }
                    }
                } else {
                    let _ = stream
                        .write_all(&json_response(404, r#"{"error":"unknown server"}"#))
                        .await;
                }
            } else {
                let _ = stream
                    .write_all(&json_response(400, r#"{"error":"invalid server id"}"#))
                    .await;
            }
        }
        _ => {
            if let Some(ref dir) = static_dir {
                // Serve static client files
                let file_path = if path == "/" { "/index.html" } else { path };
                let full_path = dir.join(file_path.trim_start_matches('/'));
                if let Ok(body) = tokio::fs::read(&full_path).await {
                    let content_type = match full_path.extension().and_then(|e| e.to_str()) {
                        Some("html") => "text/html",
                        Some("js") => "application/javascript",
                        Some("css") => "text/css",
                        Some("json") => "application/json",
                        Some("wasm") => "application/wasm",
                        Some("png") => "image/png",
                        Some("svg") => "image/svg+xml",
                        Some("ico") => "image/x-icon",
                        _ => "application/octet-stream",
                    };
                    let _ = stream
                        .write_all(&http_response(200, content_type, &body))
                        .await;
                } else {
                    // SPA fallback: serve index.html for unknown paths
                    let index = dir.join("index.html");
                    if let Ok(body) = tokio::fs::read(&index).await {
                        let _ = stream
                            .write_all(&http_response(200, "text/html", &body))
                            .await;
                    } else {
                        let _ = stream
                            .write_all(&http_response(404, "text/plain", b"Not Found"))
                            .await;
                    }
                }
            } else {
                // No static dir -- serve coordinator dashboard
                let resp = http_response(200, "text/html", COORDINATOR_DASHBOARD.as_bytes());
                let _ = stream.write_all(&resp).await;
            }
        }
    }
    let _ = stream.shutdown().await;
}

/// HTTP(S) server for client assignment, server list, and coordinator dashboard.
/// When `static_dir` is set, serves client files from that directory on non-API paths.
/// When `tls_acceptor` is provided, serves HTTPS; otherwise serves plain HTTP.
async fn run_assignment_server(
    port: u16,
    registry: Arc<RwLock<Registry>>,
    static_dir: Option<PathBuf>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
) {
    let listener = TcpListener::bind(shared::listen_addr(port))
        .await
        .expect("Failed to bind HTTP assignment port");
    let static_dir = static_dir.map(Arc::new);
    let scheme = if tls_acceptor.is_some() {
        "HTTPS"
    } else {
        "HTTP"
    };
    if let Some(ref dir) = static_dir {
        tracing::info!("Client assignment {scheme} on port {port} (serving client from {dir:?})");
    } else {
        tracing::info!("Client assignment {scheme} on port {port}");
    }

    loop {
        let (tcp_stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let reg = Arc::clone(&registry);
        let static_dir = static_dir.clone();

        if let Some(ref acceptor) = tls_acceptor {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor.accept(tcp_stream).await {
                    Ok(tls_stream) => {
                        handle_http_request(tls_stream, reg, static_dir).await;
                    }
                    Err(e) => {
                        tracing::debug!("TLS handshake failed: {e}");
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                handle_http_request(tcp_stream, reg, static_dir).await;
            });
        }
    }
}
