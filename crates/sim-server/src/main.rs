mod world;
mod simulation;
mod network;

use clap::Parser;
use shared::entity::ServerId;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

#[derive(Parser, Debug)]
#[command(name = "sim-server", about = "Game simulation server")]
struct Args {
    /// Unique server ID
    #[arg(long, default_value_t = 1)]
    server_id: u16,

    /// WebSocket port for client connections
    #[arg(long, default_value_t = 9001)]
    port: u16,

    /// UDP port for peer server communication (Phase 2)
    #[arg(long, default_value_t = 9101)]
    udp_port: u16,

    /// Number of NPCs to spawn initially
    #[arg(long, default_value_t = 20)]
    initial_npcs: u32,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let server_id = ServerId(args.server_id);
    tracing::info!("Sim-server {} starting on port {}", server_id, args.port);

    // Create the world
    let mut world = world::World::new(server_id);

    // Spawn initial NPCs at random positions
    world.spawn_initial_npcs(args.initial_npcs);
    tracing::info!("Spawned {} initial NPCs", args.initial_npcs);

    let world = Arc::new(RwLock::new(world));

    // Channel for client inputs -> game loop
    let (input_tx, input_rx) = mpsc::unbounded_channel();

    // Start WebSocket server
    let ws_world = Arc::clone(&world);
    let ws_handle = tokio::spawn(network::run_websocket_server(
        args.port,
        ws_world,
        input_tx,
    ));

    // Run the game loop
    let sim_handle = tokio::spawn(simulation::run_game_loop(world, input_rx));

    tokio::select! {
        r = ws_handle => {
            if let Err(e) = r { tracing::error!("WebSocket server error: {e}"); }
        }
        r = sim_handle => {
            if let Err(e) = r { tracing::error!("Game loop error: {e}"); }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down");
        }
    }
}
