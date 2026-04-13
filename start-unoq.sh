#!/usr/bin/env bash
set -e

# UNO Q Mesh Deployment (True Peer-to-Peer)
# Coordinator + client on zzttpi (Pi Zero W2), sim-servers on UNO Q boards
# Clients connect directly to sim-servers (no gateway)
# Board dashboard on :3000 with mesh server widget + start/stop toggle
# Mac is not required — entire mesh runs on embedded hardware

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids-unoq"

UNOQ=unoq
UNOQ2=unoq2
UNOQ3=unoq3
ZZTTPI=zzttpi
UNOQ_IP=192.168.0.15
UNOQ2_IP=192.168.0.173
UNOQ3_IP=192.168.0.86
ZZTTPI_IP=192.168.0.208

# Check if already running (coordinator on zzttpi)
if ssh $ZZTTPI 'pgrep -f coordinator' >/dev/null 2>&1; then
  echo "Already running. Run ./stop-unoq.sh first."
  exit 1
fi

echo "=== Coordinator: $ZZTTPI ($ZZTTPI_IP) ==="
echo ""

# --- Generate TLS certificate if needed ---
CERT_DIR="$DIR/certs"
CERT_FILE="$CERT_DIR/mesh-coordinator.crt"
KEY_FILE="$CERT_DIR/mesh-coordinator.key"

if [ ! -f "$CERT_FILE" ] || [ ! -f "$KEY_FILE" ]; then
  echo "=== Generating self-signed TLS certificate ==="
  mkdir -p "$CERT_DIR"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
    -days 3650 -nodes \
    -subj '/CN=mesh-coordinator' \
    -addext "subjectAltName=IP:${ZZTTPI_IP},IP:${UNOQ_IP},IP:${UNOQ2_IP},IP:${UNOQ3_IP},DNS:zzttpi.local,DNS:zzttpi,DNS:unoq.local,DNS:unoq2.local,DNS:unoq3.local" \
    -keyout "$KEY_FILE" -out "$CERT_FILE" 2>/dev/null
  echo "  Certificate generated with SANs for all boards"
  echo "  To trust it: sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain $CERT_FILE"
  echo ""
fi

# --- Cross-compile if needed ---
RELEASE_DIR_ARM64="$DIR/target/aarch64-unknown-linux-gnu/release"
RELEASE_DIR_ARMV7="$DIR/target/armv7-unknown-linux-gnueabihf/release"
NEED_ARM64=false
NEED_ARMV7=false

[ ! -f "$RELEASE_DIR_ARM64/sim-server" ] || [ ! -f "$RELEASE_DIR_ARM64/board-dash" ] && NEED_ARM64=true
[ ! -f "$RELEASE_DIR_ARMV7/coordinator" ] && NEED_ARMV7=true

# --- Build client if needed ---
if [ ! -d "$DIR/client/dist" ]; then
  echo "=== Building client ==="
  (cd "$DIR/client" && VITE_COORDINATOR_URL="" npx vite build)
  echo ""
fi

if $NEED_ARM64; then
  echo "=== Cross-compiling sim-server + board-dash for aarch64 ==="
  cross build --release --target aarch64-unknown-linux-gnu -p sim-server -p board-dash
  echo ""
fi
if $NEED_ARMV7; then
  echo "=== Cross-compiling coordinator for armv7 (Pi Zero W2) ==="
  cross build --release --target armv7-unknown-linux-gnueabihf -p coordinator
  echo ""
fi

# --- Stop dashboards so we can overwrite binaries ---
echo "=== Stopping dashboards for deploy ==="
ssh $UNOQ 'systemctl --user stop dashboard' 2>/dev/null || true
ssh $UNOQ2 'systemctl --user stop dashboard' 2>/dev/null || true
ssh $UNOQ3 'systemctl --user stop dashboard' 2>/dev/null || true

# --- Deploy binaries ---
echo "=== Deploying binaries ==="
ssh $UNOQ 'mkdir -p ~/mesh ~/mesh/certs ~/dashboard/static'
ssh $UNOQ2 'mkdir -p ~/mesh ~/mesh/certs ~/dashboard/static'
ssh $UNOQ3 'mkdir -p ~/mesh ~/mesh/certs ~/dashboard/static'
ssh $ZZTTPI 'mkdir -p ~/mesh ~/mesh/certs'

# sim-server -> boards
scp -q "$RELEASE_DIR_ARM64/sim-server" $UNOQ:~/mesh/
scp -q "$RELEASE_DIR_ARM64/sim-server" $UNOQ2:~/mesh/
scp -q "$RELEASE_DIR_ARM64/sim-server" $UNOQ3:~/mesh/

# coordinator + client + certs -> zzttpi
scp -q "$RELEASE_DIR_ARMV7/coordinator" $ZZTTPI:~/mesh/
scp -rq "$DIR/client/dist" $ZZTTPI:~/mesh/client-dist
scp -q "$CERT_FILE" "$KEY_FILE" $ZZTTPI:~/mesh/certs/

# certs -> boards (for sim-server TLS)
scp -q "$CERT_FILE" "$KEY_FILE" $UNOQ:~/mesh/certs/
scp -q "$CERT_FILE" "$KEY_FILE" $UNOQ2:~/mesh/certs/
scp -q "$CERT_FILE" "$KEY_FILE" $UNOQ3:~/mesh/certs/

# board-dash → boards
scp -q "$RELEASE_DIR_ARM64/board-dash" $UNOQ:~/dashboard/unoq-dashboard
scp -q "$RELEASE_DIR_ARM64/board-dash" $UNOQ2:~/dashboard/unoq-dashboard
scp -q "$RELEASE_DIR_ARM64/board-dash" $UNOQ3:~/dashboard/unoq-dashboard

# Static assets for the dashboard
STATIC_DIR="$DIR/crates/board-dash/static"
if [ -d "$STATIC_DIR" ] && [ "$(ls -A "$STATIC_DIR" 2>/dev/null)" ]; then
  scp -q "$STATIC_DIR"/* $UNOQ:~/dashboard/static/
  scp -q "$STATIC_DIR"/* $UNOQ2:~/dashboard/static/
  scp -q "$STATIC_DIR"/* $UNOQ3:~/dashboard/static/
fi
echo "  Binaries deployed"
echo ""

# --- Kill old processes ---
ssh $UNOQ 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ2 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ3 'pkill -f sim-server' 2>/dev/null || true
ssh $ZZTTPI 'systemctl --user stop mesh-coordinator' 2>/dev/null || true

# --- Restart dashboard service (picks up new binary) ---
echo "=== Restarting dashboards ==="
MESH_ARGS_1="--server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ_IP} --initial-npcs 15 --tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key"
MESH_ARGS_2="--server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ2_IP} --initial-npcs 15 --tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key"
MESH_ARGS_3="--server-id 3 --port 7880 --udp-port 9103 --metrics-port 8103 --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ3_IP} --initial-npcs 15 --tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key"

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

ssh $UNOQ3 "mkdir -p ~/.config/systemd/user/dashboard.service.d && cat > ~/.config/systemd/user/dashboard.service.d/mesh.conf << EOF
[Service]
Environment=MESH_BINARY=/home/arduino/mesh/sim-server
Environment=MESH_ARGS=${MESH_ARGS_3}
EOF
systemctl --user daemon-reload
systemctl --user restart dashboard"
echo "  Dashboards restarted"
echo ""

# --- Start coordinator on zzttpi (systemd service, serves client + dashboard over HTTPS) ---
echo "=== Starting coordinator on $ZZTTPI ($ZZTTPI_IP) (ws:7870 https:7871) ==="
ssh $ZZTTPI 'systemctl --user restart mesh-coordinator'
sleep 1

# --- Start sim-servers on boards ---
echo "=== Starting sim-server 1 on $UNOQ ($UNOQ_IP) ==="
ssh $UNOQ "nohup ~/mesh/sim-server \
  --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 \
  --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ_IP} \
  --initial-npcs 15 \
  --tls-cert ~/mesh/certs/mesh-coordinator.crt \
  --tls-key ~/mesh/certs/mesh-coordinator.key \
  > ~/mesh/sim1.log 2>&1 &"

echo "=== Starting sim-server 2 on $UNOQ2 ($UNOQ2_IP) ==="
ssh $UNOQ2 "nohup ~/mesh/sim-server \
  --server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 \
  --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ2_IP} \
  --initial-npcs 15 \
  --tls-cert ~/mesh/certs/mesh-coordinator.crt \
  --tls-key ~/mesh/certs/mesh-coordinator.key \
  > ~/mesh/sim2.log 2>&1 &"

echo "=== Starting sim-server 3 on $UNOQ3 ($UNOQ3_IP) ==="
ssh $UNOQ3 "nohup ~/mesh/sim-server \
  --server-id 3 --port 7880 --udp-port 9103 --metrics-port 8103 \
  --coordinator ${ZZTTPI_IP}:7870 --advertise-host ${UNOQ3_IP} \
  --initial-npcs 15 \
  --tls-cert ~/mesh/certs/mesh-coordinator.crt \
  --tls-key ~/mesh/certs/mesh-coordinator.key \
  > ~/mesh/sim3.log 2>&1 &"

sleep 2

echo ""
echo "================================================"
echo "  Coordinator + Client (zzttpi — Pi Zero W2)"
echo "    Client:     https://${ZZTTPI_IP}:7871  (self-signed TLS)"
echo "    Dashboard:  https://${ZZTTPI_IP}:7871/dashboard"
echo "    WebSocket:  ${ZZTTPI_IP}:7870"
echo ""
echo "  UNO Q boards (dashboard + sim-servers)"
echo "    Board 1: http://$UNOQ_IP:3000  (S1: wss:7878 udp:9101 metrics:8101)"
echo "    Board 2: http://$UNOQ2_IP:3000  (S2: wss:7879 udp:9102 metrics:8102)"
echo "    Board 3: http://$UNOQ3_IP:3000  (S3: wss:7880 udp:9103 metrics:8103)"
echo ""
echo "  Assignment:  curl -k https://${ZZTTPI_IP}:7871/assign"
echo "  Metrics:"
echo "    curl $UNOQ_IP:8101/metrics"
echo "    curl $UNOQ2_IP:8102/metrics"
echo "    curl $UNOQ3_IP:8103/metrics"
echo ""
echo "  NOTE: Self-signed cert — accept the browser warning on first visit."
echo "        WebGPU requires secure context (HTTPS)."
echo "================================================"
echo ""
echo "Logs:"
echo "  Coordinator: ssh $ZZTTPI 'cat ~/mesh/coordinator.log'"
echo "  Board logs:  ssh $UNOQ 'cat ~/mesh/sim1.log'"
echo "Stop: ./stop-unoq.sh"
