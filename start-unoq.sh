#!/usr/bin/env bash
set -e

# UNO Q Mesh Deployment (True Peer-to-Peer)
# Coordinator + client on Mac, sim-servers on UNO Q boards
# Clients connect directly to sim-servers (no gateway)
# Board dashboard on :3000 with mesh server widget + start/stop toggle

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
if [ ! -f "$RELEASE_DIR/sim-server" ] || [ ! -f "$RELEASE_DIR/board-dash" ]; then
  echo "=== Cross-compiling for aarch64 ==="
  cross build --release --target aarch64-unknown-linux-gnu -p sim-server -p board-dash
  echo ""
fi

# --- Deploy binaries ---
echo "=== Deploying binaries to boards ==="
ssh $UNOQ 'mkdir -p ~/mesh ~/dashboard/static'
ssh $UNOQ2 'mkdir -p ~/mesh ~/dashboard/static'

# sim-server → ~/mesh/
scp -q "$RELEASE_DIR/sim-server" $UNOQ:~/mesh/
scp -q "$RELEASE_DIR/sim-server" $UNOQ2:~/mesh/

# board-dash → ~/dashboard/unoq-dashboard (replaces old dashboard)
scp -q "$RELEASE_DIR/board-dash" $UNOQ:~/dashboard/unoq-dashboard
scp -q "$RELEASE_DIR/board-dash" $UNOQ2:~/dashboard/unoq-dashboard

# Static assets for the dashboard
STATIC_DIR="$DIR/crates/board-dash/static"
if [ -d "$STATIC_DIR" ] && [ "$(ls -A "$STATIC_DIR" 2>/dev/null)" ]; then
  scp -q "$STATIC_DIR"/* $UNOQ:~/dashboard/static/
  scp -q "$STATIC_DIR"/* $UNOQ2:~/dashboard/static/
fi
echo "  Binaries deployed"
echo ""

# --- Kill old sim-servers on boards ---
ssh $UNOQ 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ2 'pkill -f sim-server' 2>/dev/null || true

# --- Restart dashboard service (picks up new binary) ---
echo "=== Restarting dashboards ==="
MESH_ARGS_1="--server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 --coordinator ${MAC_IP}:7870 --advertise-host ${UNOQ_IP} --initial-npcs 15"
MESH_ARGS_2="--server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 --coordinator ${MAC_IP}:7870 --advertise-host ${UNOQ2_IP} --initial-npcs 15"

# Set env vars in the systemd override so the dashboard knows how to start sim-server
ssh $UNOQ "mkdir -p ~/.config/systemd/user/dashboard.service.d && cat > ~/.config/systemd/user/dashboard.service.d/mesh.conf << EOF
[Service]
Environment=MESH_BINARY=/home/arduino/mesh/sim-server
Environment=MESH_ARGS=${MESH_ARGS_1}
EOF
systemctl --user daemon-reload
systemctl --user restart dashboard"

ssh $UNOQ2 "mkdir -p ~/.config/systemd/user/dashboard.service.d && cat > ~/.config/systemd/user/dashboard.service.d/mesh.conf << EOF
[Service]
Environment=MESH_BINARY=/home/arduino/mesh/sim-server
Environment=MESH_ARGS=${MESH_ARGS_2}
EOF
systemctl --user daemon-reload
systemctl --user restart dashboard"
echo "  Dashboards restarted"
echo ""

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
echo "  UNO Q boards (dashboard + sim-servers)"
echo "    Board 1: http://$UNOQ_IP:3000  (S1: ws:7878 udp:9101 metrics:8101)"
echo "    Board 2: http://$UNOQ2_IP:3000  (S2: ws:7879 udp:9102 metrics:8102)"
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
