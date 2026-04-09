import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { spawn } from "node:child_process";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, "../..");

const ORACLE_SYSTEM_PROMPT = `You are the codebase oracle for the ZZServerMeshing project. Answer the question by reading actual code and tests — they are the ultimate source of truth. Cite file:line for every claim. If docs contradict code, say so. Be precise and concise.`;

function askClaude(question: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const proc = spawn(
      "claude",
      [
        "-p",
        "--output-format",
        "text",
        "--allowedTools",
        "Read,Glob,Grep",
        "--append-system-prompt",
        ORACLE_SYSTEM_PROMPT,
        question,
      ],
      {
        cwd: REPO_ROOT,
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env },
      }
    );

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
        reject(new Error(`claude exited with code ${code}: ${stderr}`));
      }
    });

    proc.on("error", (err) => {
      reject(new Error(`Failed to spawn claude: ${err.message}`));
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
      const answer = await askClaude(question);
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
