---
name: test-gaps
description: Hunt down test coverage gaps in the server meshing system, write tests to fill them, and verify against AKS.
user-invocable: true
---

# Test Gap Hunter

Find untested code paths, write unit and integration tests, verify they catch real bugs.

## First Steps

1. Read `.claude/skills/test-gaps/PASS_LOG.md` for history of prior runs
2. Read `.claude/skills/mesh-dev/CHANGELOG.md` for known issues and recent changes
3. Scan the codebase for existing tests and untested code

## Codebase Test Inventory

Check what exists and what doesn't:

```bash
# Rust unit tests
grep -r "#\[test\]" crates/ --include="*.rs" -l

# Rust integration tests
ls crates/*/tests/ 2>/dev/null

# TypeScript test files
find tools/ client/ -name "*.test.ts" -o -name "*.spec.ts" 2>/dev/null

# Existing test tools
ls tools/test-suite/scenarios/
ls tools/browser-test.ts tools/client-sim.ts tools/diagnose*.ts 2>/dev/null
```

## Priority Order for Coverage

Hunt gaps in this order (highest risk first):

### 1. Protocol Correctness
- Binary frame encoding/decoding roundtrip (gateway encode → client decode)
- Entity serialization: UUID bytes, f32 positions, u16 authority
- Frame type byte handling (0x01 full, 0x02 delta)
- JSON protocol messages: all `ClientMessage`, `ServerMessage`, `CoordClientMsg`, `CoordServerMsg` variants
- Edge cases: empty snapshots, max entity count, zero-length frames

### 2. Entity Authority & Transfers
- Authority transfer: source removes, target adds, no duplicates
- Recovery dedup: UUID hash distribution across N survivors
- Rebalance threshold: doesn't trigger below minimum, doesn't oscillate near boundary
- Entity dedup in gateway: overlapping servers during transfer window
- Heartbeat entity counts match actual authoritative entity count

### 3. Gateway Aggregation
- Snapshot merging from N servers
- Entity dedup prefers correct authority server
- Rebalance event detection (merged view, not per-server)
- Metrics accuracy: snapshots/sec, bandwidth, entity counts
- Broadcast skips when no new data (duplicate tick prevention)

### 4. Simulation Logic
- NPC wandering: direction changes, velocity, world bounds clamping
- Player input: MoveInput applies velocity correctly
- Spawn/remove entity lifecycle
- Shadow entity staleness cleanup
- Tick counter advancement

### 5. Client Rendering Pipeline
- Binary decoder handles all frame types
- Warmup period: first 3 snapshots snap, no interpolation
- Duplicate tick skip
- Interpolation: previousPosition from rendered position
- Entity add/remove across snapshots
- Server count rebuilding from deduplicated map

### 6. Coordinator Logic
- Server registration/unregistration
- Heartbeat timeout → crash detection
- Peer list broadcast (excludes self, excludes gateway from sim peers)
- Drain directive targets
- Recovery broadcast includes survivor list

## Writing Tests

### Rust Unit Tests
Add `#[cfg(test)] mod tests { ... }` at the bottom of the source file being tested.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_example() {
        // ...
    }
}
```

Run: `cargo test -p <crate>`

### Rust Integration Tests
Create `crates/<crate>/tests/<name>.rs` for cross-module tests.

Run: `cargo test -p <crate> --test <name>`

### TypeScript Tests
For protocol/decoder tests, create files in `tools/tests/` using Node's built-in `assert`.

Run: `npx tsx tools/tests/<name>.ts`

### End-to-End (against AKS)
Use existing tools:
```bash
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
npx tsx browser-test.ts ws://${GW_IP}
npx tsx test-suite/runner.ts ws://${GW_IP} 8101 3 --no-color --json <scenario>
```

## Workflow

### 1. Scan for Gaps
Explore each crate/module. For every public function, check:
- Does it have a unit test?
- Are edge cases covered? (empty input, max values, error paths)
- Is it only tested indirectly through integration?

### 2. Prioritize
Pick the highest-risk untested code path from the priority list above.

### 3. Write Tests
Write focused tests that:
- Test one thing each
- Have descriptive names: `test_authority_transfer_removes_from_source`
- Cover the happy path AND edge cases
- Are deterministic (no random, no timing dependence where possible)

### 4. Verify
```bash
# Rust
cargo test --workspace

# TypeScript
npx tsx tools/tests/<name>.ts

# Integration (if applicable)
npx tsx browser-test.ts ws://${GW_IP}
```

### 5. Update Pass Log
Append results to `.claude/skills/test-gaps/PASS_LOG.md` (keep last 5).

## Pass Log Format

Each entry in `.claude/skills/test-gaps/PASS_LOG.md`:

```markdown
## Run [N] — [date] — [area covered]
- **Files tested**: list of source files that got new tests
- **Tests added**: count and names
- **Gaps found**: what was untested
- **Gaps remaining**: what still needs tests
- **All tests pass**: YES/NO
- **Notes**: anything notable (bugs found by tests, etc.)
```

## Rules

1. **Never write tests that always pass** — if a test can't fail, it's useless
2. **Test behavior, not implementation** — test what a function does, not how
3. **One assertion focus per test** — test name should describe exactly what's checked
4. **No mocks for things we own** — use the real types from `shared` crate
5. **Run all tests before declaring done** — `cargo test --workspace` must pass
6. **If a test catches a real bug, fix the bug** — don't skip the test
7. **Update PASS_LOG.md** at the end of every run
