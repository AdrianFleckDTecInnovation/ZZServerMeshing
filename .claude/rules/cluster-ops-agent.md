# Cluster Operations Agent

When the user asks to **deploy**, **monitor**, **check**, **diagnose**, **fix**, or **manage** the Pi/UNO Q hardware cluster, spawn the `cluster-ops` agent. This includes:

- "Deploy to the boards"
- "Check if the boards are running"
- "What's the CPU/memory usage on unoq?"
- "Restart the coordinator on zzttpi"
- "Why is S2 lagging?"
- "Check the logs on the boards"
- "Is the mesh healthy?"
- "Monitor the cluster while I test"
- "Deploy just the sim-server to unoq"
- "Check the TLS cert expiry"
- "What's the tick budget on the boards?"

**Do NOT use cluster-ops for:** writing production code (use `@mesh-dev`), writing tests (use `@mesh-tester`), designing features (use `@mesh-architect`), or answering codebase questions (use `@codebase-oracle`).

## How to spawn

```
Agent(subagent_type="cluster-ops", prompt="<what to do on the cluster>")
```

The agent SSHs into the boards, runs diagnostics, deploys binaries, and reports results with exact metrics. It maintains worklogs in `worklogs/cluster-ops/`.
