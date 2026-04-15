---
name: Tiered replication load test results (2026-04-15)
description: UNO Q cluster load test results after tiered AoI shadow replication — scaling limits, breakdown points, and headroom analysis
type: project
---

Tiered AoI-aware shadow replication (design 010) deployed and load-tested on UNO Q cluster (2026-04-15).

**Bandwidth savings:** 70-79% consistently across all entity counts.

**Scaling limits on 3 UNO Q boards:**
- Comfortable operation: up to ~8,000 total entities (2,666/server)
- Network telemetry starts dropping: ~8,000+
- Peer connection instability: ~9,600-10,000 entities
- Still "runs" but not functionally well: ~15,000 entities
- Projected hard ceiling: ~21,000 total (7,000 auth/server) at 80% tick budget

**Hardware usage at 14,500 entities:** 19-24% CPU, 16-23MB RAM, 47-50C temp per board. Massive headroom for game logic — simulation is NOT the bottleneck, network/WiFi saturation is.

**Why:** The bottleneck shifted from simulation to WiFi. At ~8k+ entities the UDP replication volume exceeds what the WiFi links can sustain, causing telemetry drops and eventually peer connection loss. CPU and RAM are barely touched.

**How to apply:** When planning entity budgets or game logic complexity, the boards have significant CPU/RAM headroom. Network is the constraint. Future optimizations should target UDP packet efficiency (delta compression, better chunking) rather than simulation performance.
