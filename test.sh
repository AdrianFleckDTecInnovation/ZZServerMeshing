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
source "$DIR/config.sh"

NUM_SERVERS="${NUM_SERVERS:-$DEFAULT_NUM_SERVERS}"

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
READY=false

for i in $(seq 1 30); do
  # Check if coordinator assignment endpoint is up
  if curl -sf "http://localhost:$COORD_HTTP_PORT/assign" > /dev/null 2>&1; then
    # Check if at least one sim-server metrics endpoint is up
    if curl -sf "http://localhost:$(sim_metrics_port 1)/metrics" > /dev/null 2>&1; then
      # Get assigned server URL and check WebSocket connectivity
      SERVER_URL=$(curl -sf "http://localhost:$COORD_HTTP_PORT/assign" | node -e "
        let d=''; process.stdin.on('data',c=>d+=c);
        process.stdin.on('end',()=>{ try{console.log(JSON.parse(d).ws_url)}catch{process.exit(1)} })
      " 2>/dev/null)
      if [ -n "$SERVER_URL" ]; then
        READY=true
        break
      fi
    fi
  fi
  echo "  waiting... (${i}/30)"
  sleep 1
done

if [ "$READY" = false ]; then
  echo "ERROR: Cluster failed to start within 30 seconds"
  echo "Check logs: .coordinator.log .sim-server-*.log"
  exit 1
fi

echo "  Cluster ready! (assigned: $SERVER_URL)"
echo ""

# ── Run test suite ──

echo "=== Running test suite ==="
cd "$DIR/tools"

ARGS="$SERVER_URL $(sim_metrics_port 1) $NUM_SERVERS"
if [ -n "$SCENARIOS" ]; then
  ARGS="$ARGS $SCENARIOS"
fi

# Run with --no-color for clean output + optional --json
npx tsx test-suite/runner.ts $ARGS --no-color $JSON_FLAG
EXIT_CODE=$?

echo ""
echo "=== Test suite finished (exit code: $EXIT_CODE) ==="
exit $EXIT_CODE
