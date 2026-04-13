---
alwaysApply: true
---

When working with libraries, frameworks, or APIs — spawn the `sdk-oracle` agent to fetch current documentation instead of relying on training data or calling Context7 directly. This includes setup questions, code generation, API references, and anything involving specific packages.

## Steps

1. Spawn `Agent(subagent_type="sdk-oracle")` with the library name and your question
2. Answer using the returned docs — include code examples and cite the version

## Why

The sdk-oracle agent maintains a local documentation cache in `sdkcache/`. By routing all lookups through it, repeated questions are served instantly from the vector index instead of re-fetching from Context7. The agent handles library ID resolution, cache writes, and vector indexing automatically.

## Fallback

If the `sdk-oracle` agent is not available (e.g., agent definition missing), fall back to calling Context7 MCP directly:

1. Call `resolve-library-id` with the library name and the user's question
2. Pick the best match — prefer exact names and version-specific IDs when a version is mentioned
3. Call `query-docs` with the selected library ID and the user's question
4. Answer using the fetched docs — include code examples and cite the version
