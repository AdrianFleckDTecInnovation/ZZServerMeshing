import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const index = join(root, 'target', 'sdk-index');
const entry = join(index, 'node_modules', 'mcp-local-rag', 'dist', 'index.js');
if (!existsSync(entry)) {
  console.error('Install the optional SDK index as described in docs/agent-team-guide.md.');
  process.exit(1);
}
process.env.BASE_DIR ??= root;
process.env.DB_PATH ??= join(index, 'lancedb');
process.env.CACHE_DIR ??= join(index, 'models');
process.chdir(root);
process.argv[1] = entry;
await import(pathToFileURL(entry).href);
