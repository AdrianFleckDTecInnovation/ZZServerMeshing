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
echo "=== Starting coordinator on port 7870 ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p coordinator -- --port 7870 \
  > "$DIR/.coordinator.log" 2>&1 &
echo "$!" > "$PIDFILE"
sleep 1

echo "=== Starting $NUM_SERVERS sim-servers ==="
for i in $(seq 1 "$NUM_SERVERS"); do
  WS_PORT=$((7877 + i))   # 7878, 7879, 7880, ...
  UDP_PORT=$((9100 + i))   # 9101, 9102, 9103, ...
  NPC_COUNT=$((30 / NUM_SERVERS + (i == 1 ? 30 % NUM_SERVERS : 0)))

  echo "  S${i}: ws=$WS_PORT udp=$UDP_PORT npcs=$NPC_COUNT"
  cargo run --manifest-path "$DIR/Cargo.toml" -p sim-server -- \
    --server-id "$i" --port "$WS_PORT" --udp-port "$UDP_PORT" \
    --coordinator "127.0.0.1:7870" --initial-npcs "$NPC_COUNT" \
    > "$DIR/.sim-server-${i}.log" 2>&1 &
  echo "$!" >> "$PIDFILE"
done
sleep 1

echo ""
echo "=== Starting gateway on port 4322 ==="
cd "$DIR/gateway"
COORDINATOR_URL="ws://127.0.0.1:7870" GATEWAY_PORT=4322 npx tsx src/index.ts \
  > "$DIR/.gateway.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

echo ""
echo "=== Starting client dev server on port 4321 ==="
cd "$DIR/client"
npx vite --port 4321 > "$DIR/.client.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

echo ""
echo "================================================"
echo "  Coordinator:  port 7870"
for i in $(seq 1 "$NUM_SERVERS"); do
  WS_PORT=$((7877 + i))
  echo "  Sim-server $i: ws=$WS_PORT udp=$((9100 + i))"
done
echo "  Gateway:      ws://localhost:4322"
echo "  Client:       http://localhost:4321"
echo "================================================"
echo ""
echo "Logs: .coordinator.log .sim-server-*.log .gateway.log .client.log"
echo "Stop: ./stop.sh"
