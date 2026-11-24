#!/usr/bin/env bash
# Re-index the sdkcache/ after manual edits or cloning a repo with a committed cache
set -euo pipefail

exec node "$(dirname "$0")/sdk-rag.mjs" ingest sdkcache/
