use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "coordinator", about = "Server mesh coordinator")]
struct Args {
    /// Port to listen on for sim-server connections
    #[arg(long, default_value_t = 9000)]
    port: u16,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    tracing::info!("Coordinator starting on port {}", args.port);
    // Phase 2: server registry, health monitoring, load balancing
    tracing::info!("Coordinator is a stub in Phase 1 -- implement in Phase 2");
    // Keep alive
    tokio::signal::ctrl_c().await.ok();
}
