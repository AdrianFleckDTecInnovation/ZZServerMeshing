# COORD-READ-TIMEOUT: 120-second coordinator read timeout delays crash detection and reconnection

## Severity: medium

## Category: operational

## Summary
The coordinator client WebSocket read timeout was changed from 30 seconds to 120 seconds. The comment justifies this by noting the coordinator may be silent for minutes on a stable cluster. However, 120 seconds means a half-open TCP connection (where the coordinator process crashes without sending a FIN) will not be detected for 2 full minutes. During this window, the sim-server has no coordinator connection: no PeerList updates, no rebalance directives, no crash recovery commands, and no new server registrations are received. The heartbeat sender will also be silently failing (sending into a dead socket), so the coordinator (if it restarts) will not see this server's heartbeats and may declare it dead.

## Evidence
`crates/coordinator/src/coordinator_client.rs:74`:
```rust
let read_timeout = std::time::Duration::from_secs(120);
```

The comment at lines 68-73 explains:
```rust
// The coordinator only sends messages on topology changes (PeerList,
// RebalanceDirective, etc). On a stable cluster it may be silent for
// minutes. 120s is long enough to avoid false timeouts while still
// detecting truly dead connections.
```

However, the coordinator sends heartbeat responses implicitly -- it does not send periodic pings. Looking at `crates/coordinator/src/main.rs`, the coordinator's WebSocket handler only sends messages when topology changes occur. There is no server-to-coordinator keepalive mechanism beyond the sim-server's 1-second heartbeat sends.

The reconnect logic at lines 150-153 sleeps 2 seconds then reconnects:
```rust
hb_task.abort();
tracing::warn!("Lost coordinator connection, will re-register...");
tokio::time::sleep(std::time::Duration::from_secs(2)).await;
```

Total worst-case time from coordinator crash to reconnection: 120s (timeout) + 2s (reconnect delay) = 122 seconds.

Meanwhile, the coordinator's `detect_dead_servers` at `crates/coordinator/src/registry.rs:111-120` uses a 15-second heartbeat timeout. If the coordinator restarts within the sim-server's 120s timeout window, the sim-server's heartbeat sender is still writing to the old dead socket, not to the new coordinator. The coordinator will not see heartbeats from this server and will declare it dead after 15 seconds, triggering a false `RecoverEntities` broadcast.

## Impact
Scenario: Coordinator crashes and restarts after 5 seconds.
1. T+0s: Coordinator crashes. TCP connection enters half-open state (no FIN sent).
2. T+1s: Sim-server's heartbeat sender tries to send. May fail immediately (TCP RST) or silently succeed (half-open).
3. T+5s: Coordinator restarts. No sim-servers are registered.
4. T+15s: Coordinator starts checking for dead servers, but this server never registered, so it is not in the dead list. However, other servers that reconnect faster may receive RecoverEntities for this server.
5. T+120s: Sim-server's read timeout fires. It reconnects to the new coordinator at T+122s.

During the 122-second window, the sim-server operates without coordinator oversight. If another server crashes during this window, recovery cannot happen.

The previous 30-second timeout was more conservative and aligned better with the 15-second heartbeat timeout on the coordinator side.

## Recommended Fix
Use a two-pronged approach:

1. Reduce the read timeout to 30-60 seconds (previous was 30s).
2. Add a coordinator-side periodic ping. The coordinator already runs a per-server heartbeat check loop. Add a WebSocket ping (or a lightweight JSON keepalive) every 10-15 seconds. This gives the sim-server actual traffic to expect, making the read timeout meaningful.

In `crates/coordinator/src/coordinator_client.rs`:
```rust
let read_timeout = std::time::Duration::from_secs(30);
```

Or, implement a heartbeat acknowledgement on the coordinator side -- when it receives a `Heartbeat` from a sim-server, respond with a lightweight `Pong` message. This converts the unidirectional heartbeat into a bidirectional keepalive, making the read timeout reliable at any value.

If coordinator silence is a legitimate concern (e.g., coordinator is overloaded), the fallback is WebSocket-level pings. The `tokio_tungstenite` library handles ping/pong frames automatically if the server sends them.

## Effort: small

## Related Files
- crates/sim-server/src/coordinator_client.rs:74 (120s read timeout)
- crates/sim-server/src/coordinator_client.rs:150-153 (reconnect logic)
- crates/coordinator/src/registry.rs:111-120 (15s heartbeat timeout for dead detection)
- crates/coordinator/src/main.rs:270 (dead server handling)
