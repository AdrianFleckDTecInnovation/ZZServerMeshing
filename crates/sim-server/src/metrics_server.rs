use crate::metrics::MetricsCollector;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Minimal HTTP server that serves /metrics as JSON.
/// No external deps — just raw TCP + hand-crafted HTTP responses.
pub async fn run_metrics_server(port: u16, collector: Arc<Mutex<MetricsCollector>>) {
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
            // Read the request (we don't care about the contents, just drain it)
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;

            let snapshot = collector.lock().await.snapshot();
            let body = match serde_json::to_string_pretty(&snapshot) {
                Ok(b) => b,
                Err(_) => return,
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );

            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}
