# Codex in this repository

Open the repository root in Codex. [AGENTS.md](../AGENTS.md) maps the services and gives the shared Windows and Linux commands. The repository skills under `.agents/skills/` are available by name, for example `$test-gaps` or `$mesh-optimize`.

| Optional profile | Responsibility |
| --- | --- |
| `mesh-architect` | Feature designs, compatibility and work items |
| `mesh-dev` | ECS, wire protocols and client changes |
| `mesh-tester` | Unit, integration and live scenario coverage |
| `mesh-auditor` | Code and performance reviews |
| `mesh-cleanup` | Behavior-preserving refactoring |
| `cluster-ops` | Operations on a specified Pi or UNO Q deployment |
| `codebase-oracle` | Source-backed answers about the project |
| `sdk-oracle` | Version-specific upstream documentation |

Each profile is a standalone TOML file in [the project agent directory](../.codex/agents). The historical Claude instructions and pass logs are under `.claude/`.

## Optional SDK index

The project MCP configuration includes a disabled `local-rag` entry for searching previously ingested SDK documentation. If using this integration, install its Node dependencies from the repository root, for example with Node.js 24.22.1 or newer in the 24.x line.

```sh
npm install --prefix target/sdk-index --no-save --package-lock=false mcp-local-rag@0.23.3
```

Run `npm run sdk:ingest` to index documents under `sdkcache/`. Enable `mcp_servers.local-rag` in [the project configuration](../.codex/config.toml) for the trusted checkout. The package, index and model cache are local to `target/sdk-index/`. The mesh, tests and benchmarks work independently of this integration.

## Optional codebase oracle for another project

The MCP server exposes `ask_server_meshing` over stdio. It runs the locally authenticated Codex CLI with a read-only filesystem sandbox in this checkout. Build it from the repository root.

```sh
npm --prefix mcp-oracle ci
npm --prefix mcp-oracle run build
```

Register it in the consuming Codex environment. Substitute the absolute path to this checkout. Double quotes preserve paths with spaces on both systems.

```sh
codex mcp add server-meshing-oracle -- node "/absolute/path/to/ZZServerMeshing/mcp-oracle/dist/server.js"
```

On Windows use a path such as `C:/projects/ZZServerMeshing/mcp-oracle/dist/server.js`. The server finds the native Codex executable or the Windows npm installation on `PATH`. A custom native executable can be selected with `CODEX_BIN` in the MCP server environment.
