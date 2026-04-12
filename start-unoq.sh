#!/usr/bin/env bash
set -e

# UNO Q Mesh Deployment (True Peer-to-Peer)
# Coordinator + client on Mac, sim-servers on UNO Q boards
# Clients connect directly to sim-servers (no gateway)

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
if [ ! -f "$RELEASE_DIR/sim-server" ]; then
  echo "=== Cross-compiling for aarch64 ==="
  cross build --release --target aarch64-unknown-linux-gnu -p sim-server
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
echo "=== Starting coordinator on Mac (ws:7870 http:7871) ==="
cargo run --manifest-path "$DIR/Cargo.toml" -p coordinator -- --port 7870 --http-port 7871 \
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

# --- Start client on Mac ---
echo ""
echo "=== Starting client dev server on Mac (port 4321) ==="
cd "$DIR/client"
VITE_COORDINATOR_URL="http://localhost:7871" npx vite --port 4321 > "$DIR/.client.log" 2>&1 &
echo "$!" >> "$PIDFILE"
cd "$DIR"

sleep 2

echo ""
echo "================================================"
echo "  Mac (control plane + client)"
echo "    Coordinator:  localhost:7870 (ws) / 7871 (http)"
echo "    Client:       http://localhost:4321"
echo ""
echo "  UNO Q boards (sim-servers)"
echo "    Sim-server 1: $UNOQ_IP (ws:7878 udp:9101 metrics:8101)"
echo "    Sim-server 2: $UNOQ2_IP (ws:7879 udp:9102 metrics:8102)"
echo ""
echo "  Assignment:  curl localhost:7871/assign"
echo "  Metrics:"
echo "    curl $UNOQ_IP:8101/metrics"
echo "    curl $UNOQ2_IP:8102/metrics"
echo "================================================"
echo ""
echo "Logs: .coordinator.log .client.log"
echo "Board logs: ssh $UNOQ 'cat ~/mesh/sim1.log'"
echo "Stop: ./stop-unoq.sh"
