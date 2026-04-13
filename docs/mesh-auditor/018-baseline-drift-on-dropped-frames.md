# BASELINE-DRIFT: Server baseline advances even when client frame is dropped

## Severity: high

## Category: data-loss

## Summary
When the per-client mpsc channel is full, the server drops the frame via `try_send` but has already updated the client's baseline to include the delta's changes. The client never receives the dropped entities, yet the server believes the client has them. Those entities become invisible to the client until the next periodic full snapshot (up to 5 seconds later), or until they change again.

## Evidence
In `crates/sim-server/src/systems.rs:838-865`, the delta path is:

```rust
// Delta: compare against baseline
let baseline = handle.baseline.as_mut().unwrap();
let delta = compute_delta(&entities, baseline);
let frame = encode_binary_delta(tick.0, &delta.updates, &removal_ids);

// Update baseline incrementally -- THIS HAPPENS UNCONDITIONALLY
update_baseline(baseline, &delta);

// ...then later...
if let Err(e) = handle.tx.try_send(frame) {
    match e {
        TrySendError::Full(_) => {
            tracing::warn!("Client frame channel full, dropping frame");
        }
        TrySendError::Closed(_) => {}
    }
}
```

The baseline update at line 848 happens before `try_send` at line 856. If `try_send` returns `Full`, the baseline has already advanced, but the client never received the data.

The same problem exists for full snapshots: the baseline is built at line 835 before `try_send`. If the full snapshot frame is dropped, the server thinks the client has a baseline it never received.

## Impact
**Condition:** A client experiences a brief network stall (>400ms at channel size 8) or the WebSocket send pipeline backs up.

**Effect:** Entities that were new or changed in the dropped frame become invisible to the client. They will not appear in subsequent delta frames because the server's baseline already includes them. The client sees a world with missing entities.

**Recovery:** The periodic full snapshot at `FULL_SNAPSHOT_INTERVAL` (100 ticks = 5 seconds) corrects the state. But 5 seconds of missing entities is a significant visual glitch -- players or NPCs can vanish and reappear.

**Worst case:** If the full snapshot frame is ALSO dropped (channel full again at the 100-tick mark), recovery is deferred another 100 ticks. In theory, a persistently slow client could never converge.

## Recommended Fix
Move the baseline update to after the `try_send` succeeds. If the send fails, revert or skip the baseline update:

```rust
let (frame, new_baseline_update) = if send_full {
    let frame = full_frame.clone();
    (frame, None) // baseline will be replaced
} else {
    let baseline = handle.baseline.as_ref().unwrap();
    let delta = compute_delta(&entities, baseline);
    let frame = encode_binary_delta(tick.0, &delta.updates, &delta.removals);
    (frame, Some(delta))
};

match handle.tx.try_send(frame) {
    Ok(()) => {
        if send_full {
            handle.baseline = Some(build_baseline(&entities));
        } else if let Some(delta) = new_baseline_update {
            update_baseline(handle.baseline.as_mut().unwrap(), &delta);
        }
        handle.ticks_since_full += 1;
    }
    Err(TrySendError::Full(_)) => {
        // Baseline NOT updated -- next tick will re-diff against
        // the last state the client actually received
        tracing::warn!("Client frame dropped, baseline preserved");
    }
    Err(TrySendError::Closed(_)) => {}
}
```

This ensures the server's view of "what the client has" only advances when the data is actually queued for delivery.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:828-866
- crates/sim-server/src/delta.rs:91-102
- crates/sim-server/src/resources.rs:122 (PER_CLIENT_CHANNEL_SIZE = 8)
