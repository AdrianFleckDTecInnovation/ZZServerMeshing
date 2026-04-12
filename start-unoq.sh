#!/usr/bin/env bash
set -e

# UNO Q Mesh Deployment
# Sim-servers on UNO Q boards, coordinator + gateway + client on Mac

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids-unoq"

UNOQ=unoq
UNOQ2=unoq2
UNOQ_IP=192.168.0.15
UNOQ2_IP=192.168.0.173
MAC_IP=$(ipconfig getifaddr en0)

if [ -f "$PIDFILE" ]; then
  echo "Already running. Run ./stop-unoq.sh first."
  exit 1
fi

echo "=== Mac IP: $MAC_IP ==="
echo ""

# --- Cross-compile if needed ---
RELEASE_DIR="$DIR/target/aarch64-unknown-linux-gnu/release"
if [ ! -f "$RELEASE_DIR/sim-server" ] || [ ! -f "$RELEASE_DIR/coordinator" ]; then
  echo "=== Cross-compiling for aarch64 ==="
  cross build --release --target aarch64-unknown-linux-gnu
  echo ""
fi

# --- Deploy binaries ---
echo "=== Deploying binaries to boards ==="
ssh $UNOQ 'mkdir -p ~/mesh'
ssh $UNOQ2 'mkdir -p ~/mesh'
scp -q "$RELEASE_DIR/sim-server" $UNOQ:~/mesh/
scp -q "$RELEASE_DIR/sim-server" $UNOQ2:~/mesh/
echo "  Binaries deployed"
echo ""

# --- Kill old processes on boards ---
ssh $UNOQ 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ2 'pkill -f sim-server' 2>/dev/null || true

# --- Start coordinator on Mac ---
echo "=== Starting coordinator on Mac (port 7870) ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p coordinator -- --port 7870 \
  > "$DIR/.coordinator.log" 2>&1 &
echo "$!" > "$PIDFILE"
sleep 1

# --- Start sim-servers on boards ---
echo "=== Starting sim-server 1 on $UNOQ ($UNOQ_IP) ==="
ssh $UNOQ "nohup ~/mesh/sim-server \
  --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 \
  --coordinator ${MAC_IP}:7870 --advertise-host ${UNOQ_IP} \
  --initial-npcs 15 \
  > ~/mesh/sim1.log 2>&1 &"

echo "=== Starting sim-server 2 on $UNOQ2 ($UNOQ2_IP) ==="
ssh $UNOQ2 "nohup ~/mesh/sim-server \
  --server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 \
  --coordinator ${MAC_IP}:7870 --advertise-host ${UNOQ2_IP} \
  --initial-npcs 15 \
  > ~/mesh/sim2.log 2>&1 &"

sleep 2

# --- Start gateway on Mac ---
echo ""
echo "=== Starting gateway on Mac (port 4322) ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p gateway -- \
  --port 4322 --coordinator "127.0.0.1:7870" --metrics-port 8200 \
  > "$DIR/.gateway.log" 2>&1 &
echo "$!" >> "$PIDFILE"

# --- Start client on Mac ---
echo ""
echo "=== Starting client dev server on Mac (port 4321) ==="
cd "$DIR/client"
npx vite --port 4321 > "$DIR/.client.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

sleep 2

echo ""
echo "================================================"
echo "  Mac (control plane + client)"
echo "    Coordinator:  localhost:7870"
echo "    Gateway:      ws://localhost:4322 (metrics: 8200)"
echo "    Client:       http://localhost:4321"
echo ""
echo "  UNO Q boards (sim-servers)"
echo "    Sim-server 1: $UNOQ_IP (ws:7878 udp:9101 metrics:8101)"
echo "    Sim-server 2: $UNOQ2_IP (ws:7879 udp:9102 metrics:8102)"
echo ""
echo "  Metrics:"
echo "    curl $UNOQ_IP:8101/metrics"
echo "    curl $UNOQ2_IP:8102/metrics"
echo "    curl localhost:8200/metrics"
echo "================================================"
echo ""
echo "Logs: .coordinator.log .gateway.log .client.log"
echo "Board logs: ssh $UNOQ 'cat ~/mesh/sim1.log'"
echo "Stop: ./stop-unoq.sh"
