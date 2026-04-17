use crate::metrics::MetricsCollector;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");

/// Minimal HTTP server that serves a dashboard at `/` and metrics JSON at `/metrics`.
pub async fn run_metrics_server(port: u16, collector: Arc<std::sync::Mutex<MetricsCollector>>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind metrics port {port}: {e}");
            return;
        }
    };
    tracing::info!("Metrics server listening on port {port}");

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let collector = Arc::clone(&collector);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            // Parse request path from first line: "GET /path HTTP/1.1"
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");

            if path == "/metrics" {
                // JSON metrics
                let snapshot = collector.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
                let body = match serde_json::to_string_pretty(&snapshot) {
                    Ok(b) => b,
                    Err(_) => return,
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            } else {
                // HTML dashboard
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    DASHBOARD_HTML.len(), DASHBOARD_HTML
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
            let _ = stream.shutdown().await;
        });
    }
}
