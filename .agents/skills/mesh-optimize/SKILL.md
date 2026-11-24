---
name: mesh-optimize
description: Benchmark the local release mesh and investigate tick processing, replication cost and client snapshot delivery under a controlled load.
---

Run `npm run bench -- --servers 3 --npcs 3000 --seconds 15` from the root. Keep the build profile, server and entity counts, warm-up, measurement duration and number of clients identical for comparisons. Record CPU and OS. Repeat runs when comparing small changes.

Use per-server tick and snapshot rates to distinguish a slow simulation from slow client delivery. The reported phase percentiles come from each sim-server’s most recent 200 ticks. Use `/metrics` for replication counts, shadow counts and peer health. Inspect `crates/sim-server/src/game_loop.rs` to establish what time a phase includes. Hardware measurements and design notes belong to their original topology.
