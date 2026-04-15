use askama::Template;
use axum::{Json, Router, routing::{get, post}, response::Html, response::IntoResponse};
use serde::Serialize;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

// --- Templates ---

struct ProcessInfo {
    pid: String,
    user: String,
    rss_mb: f64,
    cpu_pct: String,
    mem_pct: String,
    name: String,
}

struct ServiceInfo {
    name: String,
    status: String,
    active: bool,
}

struct DiskInfo {
    mount: String,
    size: String,
    used: String,
    avail: String,
    pct: String,
}

struct ContainerInfo {
    name: String,
    image: String,
    status: String,
    ports: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    hostname: String,
    uptime: String,
    cpu_temp: String,
    mem: MemInfo,
    processes: Vec<ProcessInfo>,
    services: Vec<ServiceInfo>,
    disks: Vec<DiskInfo>,
    containers: Vec<ContainerInfo>,
}

// --- JSON API types ---

#[derive(Serialize)]
struct Stats {
    cpu: CpuStats,
    mem: MemInfo,
    temp: f64,
    load: [f64; 3],
    net: Vec<NetInterface>,
    uptime_secs: f64,
}

#[derive(Serialize)]
struct CpuStats {
    cores: Vec<CpuCore>,
}

#[derive(Serialize)]
struct CpuCore {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
}

#[derive(Serialize)]
struct MemInfo {
    total_mb: u64,
    used_mb: u64,
    free_mb: u64,
    available_mb: u64,
    buffers_mb: u64,
    cached_mb: u64,
    swap_total_mb: u64,
    swap_used_mb: u64,
}

#[derive(Serialize)]
struct NetInterface {
    name: String,
    rx_bytes: u64,
    tx_bytes: u64,
}

// --- Handlers ---

async fn index() -> impl IntoResponse {
    let hostname = read_sys("/etc/hostname").unwrap_or_else(|| "unoq".into());
    let uptime = read_sys("/proc/uptime")
        .map(|s| {
            let secs: f64 = s.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0.0);
            let hours = (secs / 3600.0) as u64;
            let mins = ((secs % 3600.0) / 60.0) as u64;
            format!("{hours}h {mins}m")
        })
        .unwrap_or_else(|| "unknown".into());
    let cpu_temp = format!("{:.1}°C", read_max_temp());
    let mem = parse_meminfo();
    let processes = get_processes();
    let services = get_services();
    let disks = get_disks();
    let containers = get_docker_containers();

    let tpl = IndexTemplate { hostname, uptime, cpu_temp, mem, processes, services, disks, containers };
    Html(tpl.render().unwrap())
}

async fn api_stats() -> Json<Stats> {
    Json(Stats {
        cpu: parse_cpu_stats(),
        mem: parse_meminfo(),
        temp: read_max_temp(),
        load: read_loadavg(),
        net: parse_net_dev(),
        uptime_secs: read_sys("/proc/uptime")
            .and_then(|s| s.split_whitespace().next().map(|v| v.parse().unwrap_or(0.0)))
            .unwrap_or(0.0),
    })
}

async fn api_processes() -> Html<String> {
    let processes = get_processes();
    let mut html = String::from(r#"<table>
    <thead><tr><th>PID</th><th>User</th><th>RSS (MB)</th><th>MEM%</th><th>CPU%</th><th>Command</th></tr></thead>
    <tbody>"#);
    for p in &processes {
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.1}</td><td>{}%</td><td>{}%</td><td><code>{}</code></td></tr>",
            p.pid, p.user, p.rss_mb, p.mem_pct, p.cpu_pct, p.name
        ));
    }
    html.push_str("</tbody></table>");
    Html(html)
}

async fn api_services() -> Html<String> {
    let services = get_services();
    let mut html = String::from(r#"<table>
    <thead><tr><th></th><th>Service</th><th>Status</th></tr></thead>
    <tbody>"#);
    for s in &services {
        let cls = if s.active { "active" } else { "inactive" };
        html.push_str(&format!(
            r#"<tr><td><span class="svc-dot {cls}"></span></td><td>{}</td><td>{}</td></tr>"#,
            s.name, s.status
        ));
    }
    html.push_str("</tbody></table>");
    Html(html)
}

async fn api_docker() -> Html<String> {
    let containers = get_docker_containers();
    let mut html = String::from(r#"<table>
    <thead><tr><th>Name</th><th>Image</th><th>Status</th><th>Ports</th></tr></thead>
    <tbody>"#);
    if containers.is_empty() {
        html.push_str(r#"<tr><td colspan="4" style="opacity:0.5">No running containers</td></tr>"#);
    } else {
        for c in &containers {
            html.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                c.name, c.image, c.status, c.ports
            ));
        }
    }
    html.push_str("</tbody></table>");
    Html(html)
}

// --- Mesh server management ---

#[derive(Serialize)]
struct MeshStatus {
    running: bool,
    metrics: Option<serde_json::Value>,
}

/// Configuration for starting the sim-server process.
pub struct MeshConfig {
    pub binary: String,
    pub args: Vec<String>,
}

async fn api_mesh_status() -> Json<MeshStatus> {
    let running = Command::new("pgrep").arg("-f").arg("sim-server")
        .output().map(|o| o.status.success()).unwrap_or(false);
    let metrics = if running { fetch_sim_metrics().await } else { None };
    Json(MeshStatus { running, metrics })
}

async fn fetch_sim_metrics() -> Option<serde_json::Value> {
    for port in [8101, 8102, 8103, 8104, 8105] {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let req = "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            if stream.write_all(req.as_bytes()).await.is_ok() {
                let mut buf = Vec::new();
                if stream.read_to_end(&mut buf).await.is_ok() {
                    let resp = String::from_utf8_lossy(&buf);
                    if let Some(idx) = resp.find("\r\n\r\n") {
                        if let Ok(v) = serde_json::from_str(&resp[idx + 4..]) {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

async fn api_mesh_start(state: axum::extract::State<Arc<Mutex<MeshConfig>>>) -> Json<serde_json::Value> {
    let cfg = state.lock().await;
    let already = Command::new("pgrep").arg("-f").arg("sim-server")
        .output().map(|o| o.status.success()).unwrap_or(false);
    if already {
        return Json(serde_json::json!({"ok": false, "error": "already running"}));
    }
    match Command::new(&cfg.binary).args(&cfg.args)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()
    {
        Ok(child) => Json(serde_json::json!({"ok": true, "pid": child.id()})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_mesh_stop() -> Json<serde_json::Value> {
    match Command::new("pkill").arg("-f").arg("sim-server").output() {
        Ok(o) if o.status.success() => Json(serde_json::json!({"ok": true})),
        _ => Json(serde_json::json!({"ok": false, "error": "not running or pkill failed"})),
    }
}

async fn api_mesh_widget() -> Html<String> {
    let running = Command::new("pgrep").arg("-f").arg("sim-server")
        .output().map(|o| o.status.success()).unwrap_or(false);

    let metrics_html = if running {
        if let Some(m) = fetch_sim_metrics().await {
            let tick = m.get("tick").and_then(|v| v.as_u64()).unwrap_or(0);
            let auth = m.get("entity_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let shadow = m.get("shadow_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let peers = m.get("peer_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let clients = m.get("connected_clients").and_then(|v| v.as_u64()).unwrap_or(0);
            let budget = m.get("tick_budget_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sid = m.get("server_id").and_then(|v| v.as_u64()).unwrap_or(0);

            // Replication
            let repl_mode = m.get("replication_mode").and_then(|v| v.as_str()).unwrap_or("-");
            let repl_hot = m.get("replication_hot_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let repl_cold = m.get("replication_cold_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let repl_full = m.get("replication_full_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let repl_saved = if repl_full > 0 {
                100 - ((repl_hot + repl_cold) * 100 / repl_full)
            } else { 0 };

            // Network health
            let repl_drops = m.get("udp_repl_drops").and_then(|v| v.as_u64()).unwrap_or(0);
            let ctrl_drops = m.get("udp_ctrl_drops").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes_out = m.get("udp_bytes_out").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes_kb = bytes_out as f64 / 1024.0;
            let drop_color = if ctrl_drops > 0 { "#e57373" } else if repl_drops > 0 { "#ffb74d" } else { "#81c784" };

            // Per-peer health
            let mut peer_rows = String::new();
            if let Some(ph) = m.get("peer_health").and_then(|v| v.as_object()) {
                for (pid, entry) in ph {
                    let stale = entry.get("staleness_ticks").and_then(|v| v.as_u64()).unwrap_or(0);
                    let rx = entry.get("packets_received").and_then(|v| v.as_u64()).unwrap_or(0);
                    let evict = entry.get("chunk_evictions").and_then(|v| v.as_u64()).unwrap_or(0);
                    let stale_color = if stale > 20 { "#e57373" } else if stale > 5 { "#ffb74d" } else { "#81c784" };
                    let evict_color = if evict > 0 { "#e57373" } else { "inherit" };
                    peer_rows.push_str(&format!(
                        r#"<tr><td><span style="color:{stale_color}">S{pid}</span></td><td>{stale}t</td><td>{rx}/5s</td><td style="color:{evict_color}">{evict}</td></tr>"#
                    ));
                }
            }

            let peer_table = if peer_rows.is_empty() {
                String::new()
            } else {
                format!(r#"
                    <div style="margin-top:0.5rem">
                        <small style="opacity:0.6">Peer Health</small>
                        <table style="width:100%;font-size:0.85rem;margin-top:0.25rem">
                            <thead><tr><th>Peer</th><th>Stale</th><th>Rx</th><th>Evict</th></tr></thead>
                            <tbody>{peer_rows}</tbody>
                        </table>
                    </div>
                "#)
            };

            format!(r#"
                <div class="grid">
                    <div><small>Server</small><br><strong>S{sid}</strong></div>
                    <div><small>Tick</small><br><strong>{tick}</strong></div>
                    <div><small>Auth</small><br><strong>{auth}</strong></div>
                    <div><small>Shadow</small><br><strong>{shadow}</strong></div>
                    <div><small>Peers</small><br><strong>{peers}</strong></div>
                    <div><small>Clients</small><br><strong>{clients}</strong></div>
                    <div><small>Budget</small><br><strong>{budget:.1}%</strong>
                        <progress value="{budget}" max="100"></progress>
                    </div>
                </div>
                <div style="margin-top:0.5rem;font-size:0.85rem">
                    <small style="opacity:0.6">Replication</small><br>
                    <span>{repl_mode}</span> &middot; hot:{repl_hot} cold:{repl_cold} / {repl_full} ({repl_saved}% saved)
                </div>
                <div style="margin-top:0.5rem;font-size:0.85rem">
                    <small style="opacity:0.6">UDP Network</small><br>
                    <span style="color:{drop_color}">repl:{repl_drops} ctrl:{ctrl_drops} drops</span> &middot; {bytes_kb:.1}KB/tick
                </div>
                {peer_table}
            "#)
        } else {
            "<p>Running but metrics unavailable</p>".to_string()
        }
    } else {
        "<p style='opacity:0.5'>Server not running</p>".to_string()
    };

    let btn = if running {
        r#"<button class="secondary" hx-post="/api/mesh/stop" hx-swap="none" hx-on::after-request="setTimeout(()=>htmx.trigger('#mesh-widget','load'),500)">Stop Server</button>"#
    } else {
        r#"<button hx-post="/api/mesh/start" hx-swap="none" hx-on::after-request="setTimeout(()=>htmx.trigger('#mesh-widget','load'),1000)">Start Server</button>"#
    };

    let dot = if running { r#"<span class="svc-dot active"></span>"# } else { r#"<span class="svc-dot inactive"></span>"# };
    let label = if running { "Running" } else { "Stopped" };

    Html(format!(r#"
        <div style="display:flex;align-items:center;gap:0.5rem;margin-bottom:0.5rem">
            {dot} <strong>{label}</strong>
            <span style="flex:1"></span>
            {btn}
        </div>
        {metrics_html}
    "#))
}

// --- System readers ---

fn read_sys(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_max_temp() -> f64 {
    let mut max_temp: f64 = 0.0;
    for i in 0..20 {
        let path = format!("/sys/class/thermal/thermal_zone{i}/temp");
        if let Some(val) = read_sys(&path) {
            if let Ok(millideg) = val.parse::<f64>() {
                let temp = millideg / 1000.0;
                if temp > max_temp {
                    max_temp = temp;
                }
            }
        } else {
            break;
        }
    }
    max_temp
}

fn read_loadavg() -> [f64; 3] {
    read_sys("/proc/loadavg")
        .map(|s| {
            let mut parts = s.split_whitespace();
            [
                parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            ]
        })
        .unwrap_or([0.0; 3])
}

fn parse_meminfo() -> MemInfo {
    let meminfo = read_sys("/proc/meminfo").unwrap_or_default();
    let get = |key: &str| -> u64 {
        meminfo.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0) / 1024
    };
    let total_mb = get("MemTotal:");
    let free_mb = get("MemFree:");
    let available_mb = get("MemAvailable:");
    let buffers_mb = get("Buffers:");
    let cached_mb = get("Cached:");
    let swap_total_mb = get("SwapTotal:");
    let swap_free = get("SwapFree:");
    MemInfo {
        total_mb,
        used_mb: total_mb.saturating_sub(available_mb),
        free_mb,
        available_mb,
        buffers_mb,
        cached_mb,
        swap_total_mb,
        swap_used_mb: swap_total_mb.saturating_sub(swap_free),
    }
}

fn parse_cpu_stats() -> CpuStats {
    let stat = read_sys("/proc/stat").unwrap_or_default();
    let cores = stat.lines()
        .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            parts.next();
            Some(CpuCore {
                user: parts.next()?.parse().ok()?,
                nice: parts.next()?.parse().ok()?,
                system: parts.next()?.parse().ok()?,
                idle: parts.next()?.parse().ok()?,
                iowait: parts.next()?.parse().ok()?,
            })
        })
        .collect();
    CpuStats { cores }
}

fn parse_net_dev() -> Vec<NetInterface> {
    let net = read_sys("/proc/net/dev").unwrap_or_default();
    net.lines()
        .skip(2)
        .filter_map(|line| {
            let line = line.trim();
            let (name, rest) = line.split_once(':')?;
            let name = name.trim();
            if name == "lo" { return None; }
            let cols: Vec<&str> = rest.split_whitespace().collect();
            Some(NetInterface {
                name: name.to_string(),
                rx_bytes: cols.first()?.parse().ok()?,
                tx_bytes: cols.get(8)?.parse().ok()?,
            })
        })
        .collect()
}

fn get_processes() -> Vec<ProcessInfo> {
    let output = Command::new("ps")
        .args(["aux", "--sort=-rss"])
        .output()
        .ok();
    let Some(output) = output else { return vec![] };
    let text = String::from_utf8_lossy(&output.stdout);

    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 11 { return None; }
            let rss_kb: f64 = cols[5].parse().unwrap_or(0.0);
            if rss_kb == 0.0 { return None; }
            Some(ProcessInfo {
                user: cols[0].to_string(),
                pid: cols[1].to_string(),
                cpu_pct: cols[2].to_string(),
                mem_pct: cols[3].to_string(),
                rss_mb: rss_kb / 1024.0,
                name: cols[10..].join(" "),
            })
        })
        .collect()
}

fn get_services() -> Vec<ServiceInfo> {
    const SERVICES: &[&str] = &[
        "arduino-router", "arduino-app-cli", "docker",
        "containerd", "avahi-daemon", "NetworkManager",
        "bluetooth", "ssh", "lightdm",
    ];

    let output = Command::new("systemctl")
        .arg("is-active")
        .args(SERVICES)
        .output()
        .ok();
    let Some(output) = output else { return vec![] };
    let text = String::from_utf8_lossy(&output.stdout);

    SERVICES.iter()
        .zip(text.lines())
        .map(|(name, status)| ServiceInfo {
            name: name.to_string(),
            status: status.to_string(),
            active: status == "active",
        })
        .collect()
}

fn get_disks() -> Vec<DiskInfo> {
    let output = Command::new("df")
        .args(["-h", "/", "/tmp"])
        .output()
        .ok();
    let Some(output) = output else { return vec![] };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut seen = std::collections::HashSet::new();

    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 6 { return None; }
            let mount = cols[5].to_string();
            if !seen.insert(mount.clone()) { return None; }
            Some(DiskInfo {
                mount,
                size: cols[1].to_string(),
                used: cols[2].to_string(),
                avail: cols[3].to_string(),
                pct: cols[4].to_string(),
            })
        })
        .collect()
}

fn get_docker_containers() -> Vec<ContainerInfo> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"])
        .output()
        .ok();
    let Some(output) = output else { return vec![] };
    let text = String::from_utf8_lossy(&output.stdout);

    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 { return None; }
            Some(ContainerInfo {
                name: parts[0].to_string(),
                image: parts[1].to_string(),
                status: parts[2].to_string(),
                ports: parts.get(3).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

pub fn router(mesh_config: Arc<Mutex<MeshConfig>>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/stats", get(api_stats))
        .route("/api/processes", get(api_processes))
        .route("/api/services", get(api_services))
        .route("/api/docker", get(api_docker))
        .route("/api/mesh/status", get(api_mesh_status))
        .route("/api/mesh/start", post(api_mesh_start))
        .route("/api/mesh/stop", post(api_mesh_stop))
        .route("/api/mesh/widget", get(api_mesh_widget))
        .with_state(mesh_config)
}
