#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids"

if [ -f "$PIDFILE" ]; then
  echo "Already running. Run ./stop.sh first."
  exit 1
fi

echo "Building Rust..."
cargo build --manifest-path "$DIR/Cargo.toml" 2>&1

echo "Starting sim-server on ws://localhost:7878..."
cargo run --manifest-path "$DIR/Cargo.toml" -p sim-server -- \
  --server-id 1 --port 7878 --initial-npcs 30 \
  > "$DIR/.sim-server.log" 2>&1 &
SIM_PID=$!

echo "Starting client dev server on http://localhost:4321..."
cd "$DIR/client"
npx vite --port 4321 > "$DIR/.client.log" 2>&1 &
CLIENT_PID=$!
cd "$DIR"

echo "$SIM_PID" > "$PIDFILE"
echo "$CLIENT_PID" >> "$PIDFILE"

echo ""
echo "  sim-server  PID=$SIM_PID  ws://localhost:7878"
echo "  client      PID=$CLIENT_PID  http://localhost:4321"
echo ""
echo "Logs: .sim-server.log / .client.log"
echo "Stop: ./stop.sh"
