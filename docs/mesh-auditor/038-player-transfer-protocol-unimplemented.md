# Player Transfer Protocol Handlers Are Stubs

## Severity: medium

## Category: protocol

## Summary
The `CoordClientMsg::PlayerTransfer` and `CoordServerMsg::PlayerTransferResult` protocol messages are defined, deserialized, and handled in both the coordinator and sim-server -- but all handlers are log-only stubs. No sim-server system sends `PlayerTransfer` after a successful two-phase transfer ACK. The coordinator logs the request but does nothing with it (no account-to-server mapping, no persistence). The sim-server logs the result but takes no action on success or failure.

## Evidence

**Coordinator handler** at coordinator/main.rs:183-196 -- log-only stub:
```rust
Ok(CoordClientMsg::PlayerTransfer { account_id, from_server, to_server }) => {
    // Placeholder: log the transfer request. Full implementation
    // requires the SQLite persistence layer (not yet built).
    tracing::info!(account_id, from_server, to_server,
        "Player transfer request (persistence not yet implemented)");
}
```

**Sim-server handler** at coordinator_client.rs:102-116 -- log-only stub:
```rust
Ok(CoordServerMsg::PlayerTransferResult { account_id, success, error }) => {
    if success {
        tracing::info!(account_id, "Player transfer confirmed by coordinator");
    } else {
        tracing::warn!(account_id, error = ..., "Player transfer rejected by coordinator");
    }
}
```

**No sender.** The `process_transfer_acks` system (systems.rs:255-280) despawns the entity on ACK but does not send `PlayerTransfer` to the coordinator. No other system sends it either. The protocol comment says "Sent after a two-phase authority transfer ACK confirms the entity arrived" but this is aspirational, not implemented.

## Impact
- **No player account tracking**: The coordinator does not know which server owns which player account. After a transfer, the `/assign` endpoint may direct a reconnecting player to the wrong server. The player would spawn fresh instead of resuming their session.
- **Incomplete feature surface**: The handlers exist and compile, but do nothing useful. A developer might add a `PlayerTransfer` send call expecting the coordinator to act on it, not realizing the coordinator just logs and discards.
- **Tests pass without functionality**: The protocol roundtrip tests (protocol.rs:614-699) verify serialization but not behavior.

## Recommended Fix

This is an incomplete feature, not a bug. Two options:

**Option A (Complete the feature):** In `process_transfer_acks`, detect player entities (check EntityKind before despawn), and if it is a player, queue a `PlayerTransfer` message to the coordinator. In the coordinator, maintain an `account_id -> server_id` map. In `/assign`, consult this map for returning players.

**Option B (Mark as TODO):** Add explicit `// TODO: WI-xxx` markers at all three sites (protocol definition, coordinator handler, sim-server handler) referencing a tracked work item. This makes the incomplete state visible.

## Effort: small (Option B), medium (Option A)

## Related Files
- crates/shared/src/protocol.rs:126-132 (PlayerTransfer definition)
- crates/shared/src/protocol.rs:157-163 (PlayerTransferResult definition)
- crates/coordinator/src/main.rs:183-196 (coordinator stub handler)
- crates/sim-server/src/coordinator_client.rs:102-116 (sim-server stub handler)
- crates/sim-server/src/systems.rs:255-280 (process_transfer_acks, no PlayerTransfer sent)
