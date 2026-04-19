# Work Items 018: Client Re-Assigns on Reconnect

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 2).
Audit finding: `docs/mesh-auditor/074-churn-audit-summary.md` (074).

Batch 1 (coordinator newest-wins) is a hard prerequisite — without it,
`/assign` can return the ghost entry. WI-180 is the client-side change;
WI-181 is verification against the live board cluster.

---

### WI-180: Re-fetch `/assign` on every reconnect attempt

**Size:** small
**Depends on:** Batch 1 (WI-170)
**Files:**
- `client/src/main.ts`
- `client/src/network.ts`

**Description:**

Today, `getServerUrl()` is a free function in `main.ts:7-14`. It is called
once at page load. `NetworkClient` stores the URL in `this.url` and
`scheduleReconnect()` (`network.ts:299-305`) always re-opens a WebSocket
to the cached `this.url`. If that server is dead or drained, the client
loops against a dead address forever.

**Fix:** move the `/assign` call onto `NetworkClient` so it can be
re-invoked on every reconnect, and call it from `scheduleReconnect()`
before constructing a new `WebSocket`.

Concrete shape:

1. In `client/src/network.ts`, add a new private method:
   ```
   private async fetchAssignment(): Promise<string | null> {
     const assignUrl = this.coordinatorUrl
       ? `${this.coordinatorUrl}/assign`
       : '/assign';
     try {
       const resp = await fetch(assignUrl);
       if (!resp.ok) return null;
       const data = await resp.json();
       return data.ws_url ?? null;
     } catch (e) {
       console.warn(`/assign fetch failed: ${e}`);
       return null;
     }
   }
   ```

2. Add a new constructor parameter `coordinatorUrl: string` and store it
   on the class. `NetworkClient` now owns knowledge of where the
   coordinator lives.

3. Rewrite `scheduleReconnect()` (`network.ts:299-305`) to await a fresh
   assignment before reconnecting. If `/assign` succeeds, update
   `this.url` to the returned value; if it fails (null), keep the
   existing `this.url` as fallback. Then call `this.connect()`.

   ```
   private scheduleReconnect(): void {
     console.log(`Reconnecting in ${this.reconnectDelay}ms...`);
     setTimeout(async () => {
       this.reconnectDelay = Math.min(
         this.reconnectDelay * 2,
         NETCODE.RECONNECT_MAX_DELAY_MS,
       );
       const fresh = await this.fetchAssignment();
       if (fresh && fresh !== this.url) {
         console.log(`/assign returned new URL: ${fresh} (was ${this.url})`);
         this.url = fresh;
       } else if (!fresh) {
         console.warn(`/assign unavailable, retrying last-known URL ${this.url}`);
       }
       this.connect();
     }, this.reconnectDelay);
   }
   ```

4. In `client/src/main.ts`, remove the free `getServerUrl()` function and
   its caller path. The initial `/assign` now happens inside
   `NetworkClient`'s constructor (see step 5). `main.ts` just constructs
   the client with the coordinator URL:

   ```
   async function main() {
     const worldView = new WorldView();
     const coordinatorUrl = import.meta.env.VITE_COORDINATOR_URL || '';
     const network = new NetworkClient(coordinatorUrl, worldView);
     ...
   }
   ```

5. In `NetworkClient`:
   - Constructor signature changes from `(url: string, worldView: WorldView)`
     to `(coordinatorUrl: string, worldView: WorldView)`. The `url` field
     is now populated asynchronously from the first `fetchAssignment()`
     call.
   - Constructor now calls an async `init()` method (fire-and-forget) that
     does `this.url = await fetchAssignment() ?? ''` then `this.connect()`.
     If the initial `/assign` fails, `connect()` is still called with an
     empty URL; the resulting WebSocket error will bounce through
     `scheduleReconnect()` which will retry `/assign` on backoff. That's
     the right behavior — don't hard-fail the page load on a blip.
   - `getServerUrl()` remains as a public accessor (used by the debug
     overlay via `main.ts:30`). It returns `this.url`. No change.

6. The `Reconnect { url }` handler at `network.ts:272-278` continues to
   work: it still sets `this.url = msg.url` directly before closing the
   socket. `scheduleReconnect` then fires, refetches `/assign` (which may
   give a different URL than the server directed), but the server's
   directive takes precedence until the next reconnect cycle. That's a
   known oddity — document it with a one-line comment: "server-directed
   URL survives until the next /assign fetch."

   Actually, reconsidering: the goal of `Reconnect { url }` in Batch 4 is
   to direct a specific player to a specific target (for rebalance). If
   scheduleReconnect then overrides that with a fresh `/assign`, we lose
   the target. Solution for this batch: if the close was triggered by a
   `Reconnect` message, skip the `fetchAssignment()` call on the very
   next reconnect. Implementation:
   ```
   private serverDirectedUrl: string | null = null;
   // In the Reconnect handler:
   this.serverDirectedUrl = msg.url;
   this.url = msg.url;
   // In scheduleReconnect:
   if (this.serverDirectedUrl) {
     this.url = this.serverDirectedUrl;
     this.serverDirectedUrl = null;
     // skip fetchAssignment this time
   } else {
     const fresh = await this.fetchAssignment();
     // ...
   }
   ```

**Tests:**

No Vitest/Jest is currently set up in `client/`. Verification is via
browser. Do NOT add a test framework for this WI alone — that's scope
creep. Verification happens in WI-181.

**Done when:**
- `cd client && npx tsc --noEmit` passes (no TypeScript errors).
- Local cluster: `./start.sh` + open `http://localhost:4321` and observe
  normal client connection. Check the browser console — log should read
  `Assigned to server: ws://...:787N` exactly as before. No visual
  regression.

---

### WI-181: Verify on UNO Q boards with real crash scenarios

**Size:** small
**Depends on:** WI-180
**Files:** none (verification only)

**Description:**

1. Deploy the new client: `./start-unoq.sh` cross-compiles the Rust
   binaries AND builds/serves the client from the coordinator (check
   `start-unoq.sh` for the client-build step — this is the existing
   workflow). Open `https://192.168.0.208:7871` in a browser.

2. **Scenario A — crashed-server migration:**
   - Open the client. Note which sim-server the client connected to
     (debug overlay shows the WS URL; e.g. `wss://192.168.0.15:7879`).
   - SSH to that board and `sudo systemctl kill -s SIGKILL
     mesh-sim-server.service`.
   - Wait for the client's exponential backoff to fire once (up to
     `RECONNECT_MAX_DELAY_MS` per `client/src/config.ts`).
   - **Expected:** browser console logs `/assign returned new URL:
     wss://192.168.0.173:7880 (was wss://192.168.0.15:7879)` (or similar).
     Client reconnects to a surviving server. Entities render again within
     2–5 seconds. No page reload.
   - Screenshot the debug overlay showing the new server URL.

3. **Scenario B — all servers dead, one returns:**
   - `sudo systemctl stop mesh-sim-server.service` on all three boards.
   - Client keeps retrying `/assign`; coordinator returns 503 or empty
     (check behavior). Console should show `/assign unavailable,
     retrying last-known URL`.
   - `sudo systemctl start mesh-sim-server.service` on unoq.
   - **Expected:** within 10 s the client's next `/assign` attempt
     succeeds and the browser reconnects. Entities rendered again.

4. **Scenario C — coordinator blip with sim-server alive:**
   - `sudo systemctl stop mesh-coordinator.service` on zzttpi.
   - Client's WebSocket to the sim-server stays up (unaffected). If
     the client-side ws is closed by force (e.g. `sudo systemctl restart
     mesh-sim-server.service` on the board), the client's `/assign` call
     fails → falls back to last-known URL → tries that URL → either
     succeeds (sim-server alive) or retries.
   - `sudo systemctl start mesh-coordinator.service`.
   - **Expected:** client continues working. This scenario confirms the
     "fallback to last-known URL" path.

5. **Scenario D — regression check on normal operation:**
   - With all services healthy, open a fresh browser tab. Verify the
     initial `/assign` + WebSocket handshake works normally and entities
     render within 1–2 seconds.

**Done when:**
- All four scenarios pass. Evidence in `worklogs/mesh-dev/` — log snippets
  or screenshots showing the `/assign returned new URL` line for Scenario
  A, and the fallback logs for Scenarios B and C.

---

## Open questions (none for Batch 2)

## Out of scope

- Adding a test framework to the client (Vitest/Jest). If we need unit
  tests for future client work, that's a separate WI.
- Changing the coordinator's `/assign` response format. It still returns
  `{"ws_url":"..."}` — the client consumer adapts, the coordinator
  stays put.
- The `Reconnect { url }` handler is wired correctly for Batch 4 but not
  exercised in this batch. WI-180 step 6 prepares the seam.
