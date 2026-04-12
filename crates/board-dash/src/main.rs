mod routes;

use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    // Mesh server config via env vars — set by start-unoq.sh
    let mesh_binary = std::env::var("MESH_BINARY")
        .unwrap_or_else(|_| "/home/arduino/mesh/sim-server".to_string());
    let mesh_args: Vec<String> = std::env::var("MESH_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mesh_config = Arc::new(Mutex::new(routes::MeshConfig {
        binary: mesh_binary,
        args: mesh_args,
    }));

    let app = Router::new()
        .merge(routes::router(mesh_config))
        .nest_service("/static", ServeDir::new("static"));

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
