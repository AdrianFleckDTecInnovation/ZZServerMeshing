#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/config.sh"

PIDFILE="$DIR/.pids"
NUM_SERVERS="${1:-$DEFAULT_NUM_SERVERS}"

if [ -f "$PIDFILE" ]; then
  echo "Already running. Run ./stop.sh first."
  exit 1
fi

echo "=== Building Rust ==="
cargo build --manifest-path "$DIR/Cargo.toml" 2>&1

echo ""
echo "=== Starting coordinator on port $COORD_WS_PORT (HTTP assignment: $COORD_HTTP_PORT) ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p coordinator -- --port "$COORD_WS_PORT" --http-port "$COORD_HTTP_PORT" \
  > "$DIR/.coordinator.log" 2>&1 &
echo "$!" > "$PIDFILE"
sleep 1

echo "=== Starting $NUM_SERVERS sim-servers ==="
for i in $(seq 1 "$NUM_SERVERS"); do
  WS_PORT=$(sim_ws_port "$i")
  UDP_PORT=$(sim_udp_port "$i")
  METRICS_PORT=$(sim_metrics_port "$i")
  NPC_COUNT=$((DEFAULT_TOTAL_NPCS / NUM_SERVERS + (i == 1 ? DEFAULT_TOTAL_NPCS % NUM_SERVERS : 0)))

  echo "  S${i}: ws=$WS_PORT udp=$UDP_PORT metrics=$METRICS_PORT npcs=$NPC_COUNT"
  cargo run --manifest-path "$DIR/Cargo.toml" -p sim-server -- \
    --server-id "$i" --port "$WS_PORT" --udp-port "$UDP_PORT" \
    --metrics-port "$METRICS_PORT" \
    --coordinator "127.0.0.1:$COORD_WS_PORT" --advertise-host "127.0.0.1" --initial-npcs "$NPC_COUNT" \
    > "$DIR/.sim-server-${i}.log" 2>&1 &
  echo "$!" >> "$PIDFILE"
done
sleep 1

echo ""
echo "=== Starting client dev server on port $CLIENT_PORT ==="
cd "$DIR/client"
npx vite --port "$CLIENT_PORT" > "$DIR/.client.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

echo ""
echo "================================================"
echo "  Coordinator:  port $COORD_WS_PORT (ws) / $COORD_HTTP_PORT (http)"
for i in $(seq 1 "$NUM_SERVERS"); do
  echo "  Sim-server $i: ws=$(sim_ws_port "$i") udp=$(sim_udp_port "$i") metrics=$(sim_metrics_port "$i")"
done
echo "  Client:       http://localhost:$CLIENT_PORT"
echo ""
echo "  Assignment:   curl localhost:$COORD_HTTP_PORT/assign"
echo "  Metrics:      curl localhost:$(sim_metrics_port 1)/metrics  (sim-server 1)"
echo "  Test suite:   cd tools && npx tsx test-suite/runner.ts"
echo "================================================"
echo ""
echo "Logs: .coordinator.log .sim-server-*.log .client.log"
echo "Stop: ./stop.sh"
