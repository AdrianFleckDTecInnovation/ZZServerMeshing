#!/usr/bin/env bash
# Re-index the sdkcache/ after manual edits or cloning a repo with a committed cache
set -euo pipefail

export BASE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
export DB_PATH="${HOME}/.local/share/mcp-local-rag/lancedb"
export CACHE_DIR="${HOME}/.local/share/mcp-local-rag/models"

echo "Ingesting sdkcache/..."
npx -y mcp-local-rag ingest sdkcache/
echo "Done. Restart Claude Code to pick up changes."
