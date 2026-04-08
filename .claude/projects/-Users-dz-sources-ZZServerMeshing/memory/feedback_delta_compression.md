---
name: Delta compression needs thorough testing
description: Delta compression caused client-side interpolation warping - disable and test carefully before re-enabling
type: feedback
---

Delta compression in the gateway caused visible entity warping on the client due to interpolation timer conflicts. The global `lastSnapshotTime` reset on delta frames caused unchanged entities to snap backward. Even after fixing the timer, residual issues remained.

**Why:** The interpolation system assumes every frame is a full state update. Delta frames that only contain changed entities break this assumption — unchanged entities get stale interpolation state.

**How to apply:** Keep delta compression disabled (FULL_SNAPSHOT_INTERVAL=1) until per-entity interpolation timers are implemented. When re-enabling, test over real network latency (not just localhost) before shipping.
