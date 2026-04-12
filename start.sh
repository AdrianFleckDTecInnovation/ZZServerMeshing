#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids"
NUM_SERVERS="${1:-3}"

if [ -f "$PIDFILE" ]; then
  echo "Already running. Run ./stop.sh first."
  exit 1
fi

echo "=== Building Rust ==="
cargo build --manifest-path "$DIR/Cargo.toml" 2>&1

echo ""
echo "=== Starting coordinator on port 7870 (HTTP assignment: 7871) ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p coordinator -- --port 7870 --http-port 7871 \
  > "$DIR/.coordinator.log" 2>&1 &
echo "$!" > "$PIDFILE"
sleep 1

echo "=== Starting $NUM_SERVERS sim-servers ==="
for i in $(seq 1 "$NUM_SERVERS"); do
  WS_PORT=$((7877 + i))   # 7878, 7879, 7880, ...
  UDP_PORT=$((9100 + i))   # 9101, 9102, 9103, ...
  METRICS_PORT=$((8100 + i)) # 8101, 8102, 8103, ...
  NPC_COUNT=$((30 / NUM_SERVERS + (i == 1 ? 30 % NUM_SERVERS : 0)))

  echo "  S${i}: ws=$WS_PORT udp=$UDP_PORT metrics=$METRICS_PORT npcs=$NPC_COUNT"
  cargo run --manifest-path "$DIR/Cargo.toml" -p sim-server -- \
    --server-id "$i" --port "$WS_PORT" --udp-port "$UDP_PORT" \
    --metrics-port "$METRICS_PORT" \
    --coordinator "127.0.0.1:7870" --advertise-host "127.0.0.1" --initial-npcs "$NPC_COUNT" \
    > "$DIR/.sim-server-${i}.log" 2>&1 &
  echo "$!" >> "$PIDFILE"
done
sleep 1

echo ""
echo "=== Starting client dev server on port 4321 ==="
cd "$DIR/client"
npx vite --port 4321 > "$DIR/.client.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

echo ""
echo "================================================"
echo "  Coordinator:  port 7870 (ws) / 7871 (http)"
for i in $(seq 1 "$NUM_SERVERS"); do
  WS_PORT=$((7877 + i))
  echo "  Sim-server $i: ws=$WS_PORT udp=$((9100 + i)) metrics=$((8100 + i))"
done
echo "  Client:       http://localhost:4321"
echo ""
echo "  Assignment:   curl localhost:7871/assign"
echo "  Metrics:      curl localhost:8101/metrics  (sim-server 1)"
echo "  Test suite:   cd tools && npx tsx test-suite/runner.ts"
echo "================================================"
echo ""
echo "Logs: .coordinator.log .sim-server-*.log .client.log"
echo "Stop: ./stop.sh"
