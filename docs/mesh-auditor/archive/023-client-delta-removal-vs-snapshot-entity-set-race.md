# DELTA-REMOVAL-RACE: Client applyDelta does not remove entities absent from delta, causing ghost entities

## Severity: high

## Category: data-loss

## Summary
When the client receives a delta frame, it only removes entities explicitly listed in the `removals` array. Entities that are not in the delta's updates AND not in the removals remain in the client's entity map untouched. If the server baseline drifts (finding #018) or an entity disappears between full snapshots without being captured in a removal, the client retains a ghost entity that no longer exists server-side. Full snapshots correct this, but the `applySnapshot` path only runs every 100 ticks.

More critically, the full snapshot path in `applySnapshot` removes entities not present in the incoming set (world-view.ts:177-180), but the delta path in `applyDelta` does NOT do this (by design). This means any entity that should be removed but is NOT included in a delta's removal list will persist client-side until the next full snapshot.

## Evidence
In `client/src/world-view.ts:196-260`, `applyDelta`:
```typescript
applyDelta(tick, updates, removals, metrics) {
    // Apply updates (new or changed entities)
    for (const raw of updates) { /* ... */ }
    // Apply removals -- ONLY explicitly listed removals
    for (const id of removals) {
        this.entities.delete(id);
    }
    // NO code to remove entities not in either list
}
```

Compare with `applySnapshot` at lines 177-180:
```typescript
// Remove entities no longer in snapshot
for (const id of this.entities.keys()) {
    if (!incomingIds.has(id)) {
        this.entities.delete(id);
    }
}
```

The server computes removals in `delta.rs:73-77` by comparing baseline keys against current IDs. This works correctly IF the baseline accurately reflects what the client has. But if the baseline drifts (per #018), the removal set will be incomplete.

**Scenario producing ghost entities:**
1. Tick 50: Server sends delta with entity E. Baseline includes E.
2. Tick 51: Server sends delta. Channel is full. Frame dropped. Baseline includes a NEW entity F that the client never received.
3. Tick 52: Entity F is removed from the server. Server computes delta: F is in baseline but not in current, so F goes in removals. Server sends removal of F.
4. Client receives removal of F but never had F -- harmless.
5. Meanwhile, entity E changed in the dropped tick 51 frame. The baseline thinks the client has E's new position. E's actual position on the client is the old one from tick 50. But since E hasn't changed further, it appears in neither updates nor removals. Ghost drift.

This scenario is tightly coupled with #018 (baseline drift on dropped frames). Fixing #018 would prevent most ghost entity cases.

## Impact
Ghost entities render at stale positions and never update. They are visually present but do not correspond to any server-side reality. At worst, a player could see phantom NPCs or players that don't exist.

Recovery happens on the next full snapshot (every 5 seconds). With the periodic full snapshot, this is bounded, but the 5-second window is noticeable.

## Recommended Fix
This finding is largely a consequence of #018. If that is fixed (baseline only advances on successful send), ghost entities become much less likely since the baseline accurately tracks client state.

As defense-in-depth, add a client-side staleness check:
```typescript
// In the interpolation loop, age-out entities not updated recently
interpolate(dt: number): void {
    const now = performance.now();
    const staleThresholdMs = 6000; // slightly longer than FULL_SNAPSHOT_INTERVAL
    for (const [id, entity] of this.entities) {
        if (now - entity.lastUpdateTime > staleThresholdMs && id !== this.localPlayerId) {
            this.entities.delete(id);
            continue;
        }
        // ... existing interpolation ...
    }
}
```

And/or have the client send `RequestSnapshot` when it detects an entity it has not received updates for in >5 seconds.

## Effort: small

## Related Files
- client/src/world-view.ts:196-260 (applyDelta -- no stale entity removal)
- client/src/world-view.ts:177-180 (applySnapshot -- removes absent entities)
- crates/sim-server/src/delta.rs:73-77 (removal computation depends on baseline accuracy)
- Finding #018 (baseline drift is the root cause)
