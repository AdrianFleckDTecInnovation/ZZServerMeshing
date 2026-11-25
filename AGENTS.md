# Working on ZZServerMeshing

Run commands from the repository root. The local mesh runs natively on Windows and Linux. Use Rust stable, the platform C linker and Node.js 22.16 or newer with npm. On Windows use the MSVC Rust toolchain, Visual Studio C++ Build Tools and the Windows SDK.

| Task | Command |
| --- | --- |
| Install client and scenario dependencies from their lockfiles | `npm run setup` |
| Start the coordinator, three sim-servers and Vite in the foreground | `npm start` |
| Run Rust tests, TypeScript checks and the client production build | `npm test` |
| Exercise input acknowledgements and peer replication on a fresh mesh | `npm run smoke` |
| Measure release binaries with three clients and 3,000 NPCs | `npm run bench -- --servers 3 --npcs 3000 --seconds 15` |
| Profile spatial layouts, cluster transitions and actual WebSocket input latency in a short release run | `npm run bench:quick` |
| Compare one and three sim-servers with the same 300 moving clients and 300 NPCs | `npm run bench:servers` |
| Compare the original browser demo with the working tree | `npm run bench:compare` |
| Exercise player density, moving clusters and directed TLS traffic | `npm run bench:clusters` and `npm run bench:cluster-wire` |

Stop the foreground launcher with Ctrl+C or `npm stop` from another terminal before using the same ports for another run. Each launch prints its own log directory under `target/`. See [the quick start](docs/quickstart.md) for endpoints, controls and individual commands.

## Infinite Stars

`crates/zz-mesh` exposes the Bevy 0.15 library. Start with the [game integration guide](crates/zz-mesh/README.md) for a standalone consumer. `examples/infinite-stars` is the multiplayer reference game. Run `cargo run --locked -p infinite-stars` for its persistent demo and `cargo test --locked -p zz-mesh -p infinite-stars` for the library contracts and real-process checks. The release workload is `cargo run --release --locked -p infinite-stars --bin mesh-bench`. The [operations guide](docs/infinite-stars.md) describes the storage boundary, peer transport, wire format and extension example.

## Where to look

- The coordinator registers servers, publishes the peer list, assigns clients via `/assign` and initiates rebalancing. See `crates/coordinator/src/registry.rs` and `main.rs`.
- Each sim-server owns entities in a common world. A Bevy ECS schedule drives movement, handoffs, replication, shadows and client snapshots at 20 Hz. Start with `crates/sim-server/src/game_loop.rs`, `schedule.rs` and `systems/`. A server ID is a one-based u16. Entity IDs are UUIDs.
- The control plane uses JSON over WebSocket. Peers replicate versioned bincode datagrams over UDP. The browser connects directly to its assigned sim-server, applies binary full/delta frames and ACKs the tick. Follow `crates/shared/src/protocol.rs`, `crates/sim-server/src/encoding.rs` and `client/src/binary-protocol.ts` when changing either end.
- Spatial and timing constants live in `crates/shared/src/spatial.rs` and `tick.rs`. Keep the client mirrors in `client/src/config.ts` in sync. Wire changes can require a peer protocol version bump in `crates/shared/src/lib.rs`.
- Use `--cold-start` to seed NPCs on a fresh mesh. On a peer restart, survivors can already own its former entities. Restart it with zero initial entities.
- Unit tests live beside their modules. `crates/sim-server/tests/input_flow.rs` exercises ECS input and transfer behavior. `tools/tests/` covers TypeScript logic and `tools/test-suite/scenarios/` exercises live services. Several historical scenarios target an SSH board cluster or manage OS processes.

## Project workflows

The five repository skills live in `.agents/skills/`. Use `$develop` for feature design and delivery, `$mesh-dev` for a scoped implementation, `$test-gaps` for coverage, `$mesh-optimize` for measured performance work and `$ship` for a requested branch release. Optional specialist profiles live in `.codex/agents/`. Their roles and the optional MCP oracle are described in the [agent guide](docs/agent-team-guide.md).

Read current code and tests first. Historical designs, audits and worklogs include older gateway and deployment architectures. The board dashboard uses Linux board interfaces. Board and cloud deployments have separate entry points and targets. Use the local mesh for development and measurement unless the task specifically names another environment.
