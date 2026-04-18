#!/usr/bin/env bash
# Deep mesh verification tests — more thorough than ./test.sh
# Runs mesh-specific scenarios that verify protocol correctness and data flow.
#
# Usage:
#   ./test-scene.sh                    # start cluster + run scene tests
#   ./test-scene.sh --no-start         # use existing cluster
#   ./test-scene.sh --json             # JSON output
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"

# Forward all flags, append scene scenario names
exec "$DIR/test.sh" "$@" mesh-verify,input-seq-echo,peer-replication,forwarded-input,multi-client,entity-lifecycle,authority-transfer,peer-restart-reconvergence,peer-restart-twice,rebalance-stuck-recovery
