#!/usr/bin/env bash
# All-in-one test runner: build → start cluster → wait for ready → run tests → stop
# Usage:
#   ./test.sh                          # run all scenarios
#   ./test.sh spawn-burst,steady-scale # run specific scenarios
#   ./test.sh --json                   # JSON output for automated parsing
#   ./test.sh --json spawn-burst       # both flags and scenarios
#   ./test.sh --docker                 # run in Docker with resource constraints
#   ./test.sh --no-start               # skip cluster start (use existing)
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
NUM_SERVERS="${NUM_SERVERS:-3}"

# Parse flags
JSON_FLAG=""
SCENARIOS=""
DOCKER_MODE=false
NO_START=false
for arg in "$@"; do
  case "$arg" in
    --json) JSON_FLAG="--json" ;;
    --docker) DOCKER_MODE=true ;;
    --no-start) NO_START=true ;;
    *) SCENARIOS="$arg" ;;
  esac
done

cleanup() {
  if [ "$DOCKER_MODE" = true ]; then
    docker compose -f "$DIR/docker-compose.test.yml" down --timeout 5 2>/dev/null || true
  elif [ "$NO_START" = false ]; then
    "$DIR/stop.sh" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

# ── Start cluster ──

if [ "$NO_START" = false ]; then
  # Stop any existing cluster
  "$DIR/stop.sh" 2>/dev/null || true
  sleep 1

  if [ "$DOCKER_MODE" = true ]; then
    echo "=== Building and starting Docker cluster ==="
    docker compose -f "$DIR/docker-compose.test.yml" up --build -d 2>&1
  else
    echo "=== Building and starting local cluster ==="
    "$DIR/start.sh" "$NUM_SERVERS" 2>&1
  fi
fi

# ── Wait for cluster ready ──

echo ""
echo "=== Waiting for cluster to be ready ==="
GATEWAY_URL="ws://localhost:4322"
READY=false

for i in $(seq 1 30); do
  # Check if gateway is accepting connections
  if node -e "
    const ws = require('ws');
    const w = new ws.WebSocket('$GATEWAY_URL');
    w.on('open', () => { w.close(); process.exit(0); });
    w.on('error', () => process.exit(1));
    setTimeout(() => process.exit(1), 1000);
  " 2>/dev/null; then
    # Check if metrics endpoint is up
    if curl -sf http://localhost:8101/metrics > /dev/null 2>&1; then
      READY=true
      break
    fi
  fi
  echo "  waiting... (${i}/30)"
  sleep 1
done

if [ "$READY" = false ]; then
  echo "ERROR: Cluster failed to start within 30 seconds"
  echo "Check logs: .coordinator.log .sim-server-*.log .gateway.log"
  exit 1
fi

echo "  Cluster ready!"
echo ""

# ── Run test suite ──

echo "=== Running test suite ==="
cd "$DIR/tools"

ARGS="$GATEWAY_URL 8101 $NUM_SERVERS"
if [ -n "$SCENARIOS" ]; then
  ARGS="$ARGS $SCENARIOS"
fi

# Run with --no-color for clean output + optional --json
npx tsx test-suite/runner.ts $ARGS --no-color $JSON_FLAG
EXIT_CODE=$?

echo ""
echo "=== Test suite finished (exit code: $EXIT_CODE) ==="
exit $EXIT_CODE
