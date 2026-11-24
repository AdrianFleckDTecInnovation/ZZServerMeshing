import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve, dirname, delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, "../..");

const ORACLE_SYSTEM_PROMPT = `You are the codebase oracle for the ZZServerMeshing project. Answer the question by reading actual code and tests — they are the ultimate source of truth. Cite file:line for every claim. If docs contradict code, say so. Be precise and concise.`;

// npm installs a .cmd shim on Windows. Invoke the package's JS entry point
// directly when a native Codex binary is unavailable on PATH.
function codexCommand(): string[] {
  if (process.env.CODEX_BIN) return [process.env.CODEX_BIN];
  if (process.platform === "win32") {
    for (const dir of (process.env.PATH ?? "").split(delimiter).filter(Boolean)) {
      const native = join(dir, "codex.exe");
      if (existsSync(native)) return [native];
      const npmCli = join(dir, "node_modules/@openai/codex/bin/codex.js");
      if (existsSync(npmCli)) return [process.execPath, npmCli];
    }
  }
  return ["codex"];
}

function askCodex(question: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const [command, ...prefix] = codexCommand();
    const proc = spawn(
      command,
      [...prefix, "-a", "never", "exec", "--sandbox", "read-only", "--ephemeral", "--color", "never", "-"],
      {
        cwd: REPO_ROOT,
        stdio: ["pipe", "pipe", "pipe"],
        windowsHide: true,
      }
    );
    proc.stdin.on("error", reject);
    proc.stdin.end(`${ORACLE_SYSTEM_PROMPT}\n\n${question}`);
    let stdout = "";
    let stderr = "";

    proc.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
    });

    proc.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });

    proc.on("close", (code) => {
      if (code === 0) {
        resolve(stdout.trim());
      } else {
        reject(new Error(`codex exited with code ${code}: ${stderr}`));
      }
    });

    proc.on("error", (err) => {
      reject(new Error(`Failed to start Codex: ${err.message}`));
    });
  });
}

const server = new McpServer({
  name: "server-meshing-oracle",
  version: "1.0.0",
});

server.tool(
  "ask_server_meshing",
  "Ask a question about the ZZServerMeshing codebase. An expert agent will read the actual code and tests to answer precisely, citing file paths and line numbers. Use for architecture questions, understanding control flow, finding where things are defined, or understanding how features work.",
  {
    question: z
      .string()
      .describe(
        "The question about the server meshing codebase, e.g. 'How does authority transfer work?' or 'Where is UDP replication implemented?'"
      ),
  },
  async ({ question }) => {
    try {
      const answer = await askCodex(question);
      return {
        content: [{ type: "text", text: answer }],
      };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return {
        content: [{ type: "text", text: `Error querying codebase oracle: ${message}` }],
        isError: true,
      };
    }
  }
);

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("server-meshing-oracle MCP server running on stdio");
}

main().catch((err) => {
  console.error("Fatal:", err);
  process.exit(1);
});
