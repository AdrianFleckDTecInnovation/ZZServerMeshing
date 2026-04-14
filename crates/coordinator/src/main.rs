mod registry;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use registry::Registry;
use shared::protocol::CoordClientMsg;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

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
    },
    Disconnected {
        server_id: u16,
    },
    Draining {
        server_id: u16,
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
            let acceptor = build_tls_acceptor(cert_path, key_path)
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
        tls_acceptor,
    ));

    // Accept sim-server WebSocket connections
    let listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind coordinator port");
    tracing::info!("Coordinator listening on port {}", args.port);

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Accept error: {e}");
                continue;
            }
        };

        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!("WS handshake error from {addr}: {e}");
                continue;
            }
        };

        tracing::info!("Sim-server connecting from {addr}");
        let event_tx = event_tx.clone();
        tokio::spawn(handle_server_connection(ws, addr, event_tx));
    }
}

async fn handle_server_connection(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
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

    // Read inbound messages
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<CoordClientMsg>(&text) {
                Ok(CoordClientMsg::Register {
                    server_id: sid,
                    udp_port,
                    ws_port,
                    host,
                    tls,
                }) => {
                    // Use advertised host if provided, otherwise use the peer's source IP
                    let resolved_host = host.unwrap_or_else(|| addr.ip().to_string());
                    server_id = Some(sid);
                    let _ = event_tx.send(RegistryEvent::Register {
                        server_id: sid,
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
                }) => {
                    let _ = event_tx.send(RegistryEvent::Heartbeat {
                        server_id: sid,
                        entity_count,
                    });
                }
                Ok(CoordClientMsg::Draining { server_id: sid }) => {
                    let _ = event_tx.send(RegistryEvent::Draining { server_id: sid });
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
                        from_server,
                        to_server,
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
        let _ = event_tx.send(RegistryEvent::Disconnected { server_id: sid });
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
                    RegistryEvent::Heartbeat { server_id, entity_count } => {
                        reg.heartbeat(server_id, entity_count);
                    }
                    RegistryEvent::Disconnected { server_id } => {
                        let had_entities = reg.server_count() > 0;
                        reg.unregister(server_id);
                        tracing::info!(
                            "Unregistered S{server_id} | {} servers remain",
                            reg.server_count()
                        );
                        reg.broadcast_peer_list();
                        // If this was an unexpected disconnect (crash), trigger recovery
                        if server_id > 0 && had_entities {
                            tracing::warn!("S{server_id} crashed — broadcasting recovery");
                            reg.broadcast_recovery(server_id);
                        }
                    }
                    RegistryEvent::Draining { server_id } => {
                        tracing::info!("S{server_id} requesting graceful drain");
                        reg.drain_server(server_id);
                    }
                }
            }
            _ = rebalance_interval.tick() => {
                let mut reg = registry.write().await;
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

/// Build a tokio-rustls TLS acceptor from PEM certificate and key files.
fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    use rustls::pki_types::PrivateKeyDer;
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("Failed to open cert file {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to parse certs: {e}"))?;
    if certs.is_empty() {
        return Err("No certificates found in cert file".into());
    }

    let key_file = std::fs::File::open(key_path)
        .map_err(|e| format!("Failed to open key file {}: {e}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("Failed to parse private key: {e}"))?
        .ok_or("No private key found in key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("Failed to build TLS config: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

const COORDINATOR_DASHBOARD: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Mesh Coordinator</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: #0a0a0a; color: #0f0; font-family: 'Courier New', monospace; padding: 20px; }
  h1 { font-size: 18px; margin-bottom: 16px; text-shadow: 0 0 6px rgba(0,255,0,0.4); }
  .summary { display: flex; gap: 24px; margin-bottom: 20px; font-size: 14px; }
  .summary .item { background: #111; border: 1px solid #1a1a1a; border-radius: 6px; padding: 14px 20px; text-align: center; }
  .summary .value { font-size: 32px; font-weight: bold; }
  .summary .label { font-size: 11px; color: #666; margin-top: 4px; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px; margin-bottom: 20px; }
  .card { background: #111; border: 1px solid #1a1a1a; border-radius: 6px; padding: 16px; }
  .card.offline { border-color: #f44; opacity: 0.5; }
  .card h3 { font-size: 14px; margin-bottom: 10px; display: flex; justify-content: space-between; align-items: center; }
  .card h3 a { color: #4fc3f7; text-decoration: none; font-size: 11px; }
  .card h3 a:hover { text-decoration: underline; }
  .card .stats { display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 8px; font-size: 12px; }
  .card .stat-val { font-size: 20px; font-weight: bold; }
  .card .stat-lbl { color: #666; font-size: 10px; }
  .gauge-sm { width: 80px; height: 80px; margin: 0 auto 8px; }
  .phase-bar { display: flex; height: 8px; border-radius: 4px; overflow: hidden; margin-top: 10px; background: #1a1a1a; }
  .phase-bar div { height: 100%; transition: width 0.3s; }
  .phase-legend { display: flex; gap: 10px; margin-top: 6px; font-size: 10px; color: #888; flex-wrap: wrap; }
  .phase-legend span::before { content: ''; display: inline-block; width: 8px; height: 8px; border-radius: 2px; margin-right: 3px; vertical-align: middle; }
  .tick-sync { font-size: 12px; color: #888; margin-top: 8px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; margin-right: 4px; }
  .dot.ok { background: #0f0; box-shadow: 0 0 4px #0f0; }
  .dot.warn { background: #ffb74d; box-shadow: 0 0 4px #ffb74d; }
  .dot.bad { background: #f44; box-shadow: 0 0 4px #f44; }
  #error { color: #f44; font-size: 12px; margin-top: 8px; }
</style>
</head>
<body>
<h1>Mesh Coordinator</h1>

<div class="summary">
  <div class="item"><div class="value" id="server-count">-</div><div class="label">Servers</div></div>
  <div class="item"><div class="value" id="total-entities">-</div><div class="label">Total Entities</div></div>
  <div class="item"><div class="value" id="total-shadows">-</div><div class="label">Total Shadows</div></div>
  <div class="item"><div class="value" id="tick-drift">-</div><div class="label">Tick Drift</div></div>
</div>

<div class="grid" id="cards"></div>
<div id="error"></div>

<script>
const PHASE_COLORS = {
  input_drain_ms: '#4fc3f7',
  simulation_ms: '#81c784',
  transfers_ms: '#ffb74d',
  replication_ms: '#ba68c8',
  shadow_cleanup_ms: '#e57373',
};
const PHASE_LABELS = {
  input_drain_ms: 'Input',
  simulation_ms: 'Sim',
  transfers_ms: 'Transfer',
  replication_ms: 'Repl',
  shadow_cleanup_ms: 'Shadow',
};

let servers = [];

function drawGaugeSm(canvas, pct) {
  const ctx = canvas.getContext('2d');
  const cx = 40, cy = 40, r = 32;
  ctx.clearRect(0, 0, 80, 80);
  ctx.beginPath();
  ctx.arc(cx, cy, r, 0.75 * Math.PI, 0.25 * Math.PI);
  ctx.lineWidth = 8; ctx.strokeStyle = '#1a1a1a'; ctx.stroke();
  const span = 1.5 * Math.PI;
  const end = 0.75 * Math.PI + span * Math.min(pct / 100, 1);
  ctx.beginPath();
  ctx.arc(cx, cy, r, 0.75 * Math.PI, end);
  ctx.lineWidth = 8; ctx.lineCap = 'round';
  ctx.strokeStyle = pct < 50 ? '#0f0' : pct < 80 ? '#ffb74d' : '#f44';
  ctx.stroke();
  ctx.fillStyle = '#fff'; ctx.font = 'bold 14px Courier New';
  ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
  ctx.fillText(pct.toFixed(0) + '%', cx, cy);
}

function renderCard(s, metrics) {
  const offline = !metrics;
  const m = metrics || {};
  const budget = m.tick_budget_pct ?? 0;
  const phases = m.phases || {};
  const totalMs = phases.total_ms?.p95 ?? 0;

  // Phase bar widths
  let phaseBar = '';
  let legend = '';
  const budgetMs = 50;
  for (const [key, color] of Object.entries(PHASE_COLORS)) {
    const val = phases[key]?.p95 ?? 0;
    const pct = (val / budgetMs) * 100;
    phaseBar += `<div style="width:${pct}%;background:${color}"></div>`;
    legend += `<span style="--c:${color}"><span style="background:${color};width:8px;height:8px;border-radius:2px;display:inline-block;margin-right:3px;vertical-align:middle"></span>${PHASE_LABELS[key]} ${val.toFixed(1)}ms</span>`;
  }

  const peerStatus = (m.peer_count ?? 0) > 0
    ? `<span class="dot ok"></span>${m.peer_count} peer${m.peer_count > 1 ? 's' : ''}`
    : `<span class="dot bad"></span>No peers`;

  return `<div class="card${offline ? ' offline' : ''}">
    <h3>
      <span>${offline ? '<span class="dot bad"></span>' : '<span class="dot ok"></span>'} S${s.server_id} — ${s.host}</span>
      <a href="http://${s.host}:${s.metrics_port}/" target="_blank">dashboard ↗</a>
    </h3>
    <div style="display:flex;align-items:center;gap:16px">
      <canvas class="gauge-sm" id="gauge-${s.server_id}" width="80" height="80"></canvas>
      <div class="stats" style="flex:1">
        <div><div class="stat-val">${m.entity_count ?? '-'}</div><div class="stat-lbl">Auth</div></div>
        <div><div class="stat-val">${m.shadow_count ?? '-'}</div><div class="stat-lbl">Shadow</div></div>
        <div><div class="stat-val">${m.tick ?? '-'}</div><div class="stat-lbl">Tick</div></div>
      </div>
    </div>
    <div style="font-size:12px;margin-top:8px">${peerStatus} · Total p95: ${totalMs.toFixed(2)}ms</div>
    <div class="phase-bar">${phaseBar}</div>
    <div class="phase-legend">${legend}</div>
  </div>`;
}

async function fetchServers() {
  try {
    const res = await fetch('/servers');
    servers = (await res.json()).servers || [];
  } catch (e) {
    document.getElementById('error').textContent = 'Failed to fetch server list: ' + e.message;
  }
}

async function poll() {
  if (servers.length === 0) await fetchServers();

  const results = await Promise.all(servers.map(async (s) => {
    try {
      const res = await fetch(`http://${s.host}:${s.metrics_port}/metrics`, { signal: AbortSignal.timeout(2000) });
      return { server: s, metrics: await res.json() };
    } catch {
      return { server: s, metrics: null };
    }
  }));

  // Summary
  let totalEntities = 0, totalShadows = 0, ticks = [];
  for (const { metrics: m } of results) {
    if (m) {
      totalEntities += m.entity_count;
      totalShadows += m.shadow_count;
      ticks.push(m.tick);
    }
  }
  const drift = ticks.length > 1 ? Math.max(...ticks) - Math.min(...ticks) : 0;

  document.getElementById('server-count').textContent = servers.length;
  document.getElementById('total-entities').textContent = totalEntities;
  document.getElementById('total-shadows').textContent = totalShadows;
  const driftEl = document.getElementById('tick-drift');
  driftEl.textContent = drift;
  driftEl.style.color = drift < 5 ? '#0f0' : drift < 20 ? '#ffb74d' : '#f44';

  // Cards
  document.getElementById('cards').innerHTML = results.map(r => renderCard(r.server, r.metrics)).join('');

  // Draw gauges after DOM update
  for (const { server: s, metrics: m } of results) {
    const canvas = document.getElementById(`gauge-${s.server_id}`);
    if (canvas) drawGaugeSm(canvas, m?.tick_budget_pct ?? 0);
  }

  document.getElementById('error').textContent = '';
}

// Refresh server list every 10s, metrics every 1s
fetchServers().then(() => poll());
setInterval(poll, 1000);
setInterval(fetchServers, 10000);
</script>
</body>
</html>"##;

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

            let (status, body) = match assignment {
                Some(ws_url) => (
                    "200 OK",
                    format!(r#"{{"ws_url":"{}"}}"#, ws_url),
                ),
                None => (
                    "503 Service Unavailable",
                    r#"{"error":"no servers available"}"#.to_string(),
                ),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        "/servers" => {
            let r = registry.read().await;
            let list = r.get_server_list();
            drop(r);

            let body = serde_json::json!({ "servers": list }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        "/dashboard" => {
            // Always serve coordinator dashboard at /dashboard
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                COORDINATOR_DASHBOARD.len(), COORDINATOR_DASHBOARD
            );
            let _ = stream.write_all(response.as_bytes()).await;
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
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                         Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                } else {
                    // SPA fallback: serve index.html for unknown paths
                    let index = dir.join("index.html");
                    if let Ok(body) = tokio::fs::read(&index).await {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.write_all(&body).await;
                    } else {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    }
                }
            } else {
                // No static dir -- serve coordinator dashboard
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    COORDINATOR_DASHBOARD.len(), COORDINATOR_DASHBOARD
                );
                let _ = stream.write_all(response.as_bytes()).await;
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
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind HTTP assignment port");
    let static_dir = static_dir.map(Arc::new);
    let scheme = if tls_acceptor.is_some() { "HTTPS" } else { "HTTP" };
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
