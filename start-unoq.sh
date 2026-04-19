#!/usr/bin/env bash
set -e

# UNO Q Mesh Deployment (True Peer-to-Peer)
# Coordinator + client on zzttpi (Pi Zero W2), sim-servers on UNO Q boards
# Clients connect directly to sim-servers (no gateway)
# Board dashboard on :3000 with mesh server widget + start/stop toggle
# Mac is not required — entire mesh runs on embedded hardware

DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/config.sh"
source "$DIR/boards.env"

PIDFILE="$DIR/.pids-unoq"

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
  cargo zigbuild --release --target aarch64-unknown-linux-gnu -p sim-server -p board-dash
  echo ""
fi
if $NEED_ARMV7; then
  echo "=== Cross-compiling coordinator for armv7 (Pi Zero W2) ==="
  cargo zigbuild --release --target armv7-unknown-linux-gnueabihf -p coordinator
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

# --- Build MESH_ARGS from config ---
TLS_ARGS="--tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key"
BOARD_IPS=("$UNOQ_IP" "$UNOQ2_IP" "$UNOQ3_IP")
BOARD_HOSTS=("$UNOQ" "$UNOQ2" "$UNOQ3")

# --- Restart dashboard service (picks up new binary) ---
echo "=== Restarting dashboards ==="
for i in 1 2 3; do
  # MESH_ARGS is used by the board dashboard's "Start Server" button, which
  # is semantically a RESTART path (survivors already hold this server's
  # entities, so re-spawning initial NPCs would duplicate them). Do NOT add
  # --cold-start here. The initial-npcs value is still present so that a
  # future operator who explicitly wants a fresh spawn can add --cold-start
  # manually.
  # WI-223: --coordinator-tls is REQUIRED here too. The coordinator on
  # zzttpi serves WSS on :$COORD_WS_PORT and plain WS would fail the
  # handshake. Unlike --cold-start this is not semantic-specific to the
  # bootstrap vs. restart path — TLS is always required on the board mesh.
  MESH_ARGS="--server-id $i --port $(sim_ws_port $i) --udp-port $(sim_udp_port $i) --metrics-port $(sim_metrics_port $i) --coordinator ${ZZTTPI_IP}:${COORD_WS_PORT} --advertise-host ${BOARD_IPS[$((i-1))]} --initial-npcs $UNOQ_NPCS_PER_SERVER --coordinator-tls $TLS_ARGS"

  ssh ${BOARD_HOSTS[$((i-1))]} "mkdir -p ~/.config/systemd/user/dashboard.service.d && cat > ~/.config/systemd/user/dashboard.service.d/mesh.conf << EOF
[Service]
Environment=MESH_BINARY=/home/arduino/mesh/sim-server
Environment=MESH_ARGS=${MESH_ARGS}
EOF
systemctl --user daemon-reload
systemctl --user restart dashboard"
done
echo "  Dashboards restarted"
echo ""

# --- Start coordinator on zzttpi (systemd service, serves client + dashboard over HTTPS) ---
echo "=== Starting coordinator on $ZZTTPI ($ZZTTPI_IP) (ws:$COORD_WS_PORT https:$COORD_HTTP_PORT) ==="
ssh $ZZTTPI 'systemctl --user restart mesh-coordinator'
sleep 1

# --- Start sim-servers on boards ---
# WI-215: --cold-start is passed here because start-unoq.sh is the cluster
# bootstrap entry point. A subsequent service-restart (systemd, dashboard
# Start button, or manual rerun) MUST NOT pass --cold-start — survivors
# already hold the restarted server's entities and re-spawning initial_npcs
# would duplicate them on every cycle.
#
# Note: rerunning this script after an initial bootstrap will ALSO pass
# --cold-start, which can duplicate entities. Treat `./start-unoq.sh` as a
# one-shot bootstrap, not a restart tool. The board dashboard's Stop/Start
# buttons, the systemd-based dashboard-driven start path, and an
# operator re-running `~/mesh/sim-server` directly all omit --cold-start.
for i in 1 2 3; do
  echo "=== Starting sim-server $i on ${BOARD_HOSTS[$((i-1))]} (${BOARD_IPS[$((i-1))]}) ==="
  # WI-223 (design 016, Batch 6): --coordinator-tls opts into WSS for the
  # outbound coordinator control channel, trusting the same self-signed cert
  # the sim-server presents on its client-facing port. Paired with the
  # coordinator also binding TLS on :$COORD_WS_PORT (via $TLS_ARGS on the
  # coordinator side, which the systemd unit picks up).
  ssh ${BOARD_HOSTS[$((i-1))]} "nohup ~/mesh/sim-server \
    --server-id $i --port $(sim_ws_port $i) --udp-port $(sim_udp_port $i) --metrics-port $(sim_metrics_port $i) \
    --coordinator ${ZZTTPI_IP}:${COORD_WS_PORT} --advertise-host ${BOARD_IPS[$((i-1))]} \
    --initial-npcs $UNOQ_NPCS_PER_SERVER \
    --cold-start \
    --coordinator-tls \
    $TLS_ARGS \
    > ~/mesh/sim${i}.log 2>&1 &"
done

sleep 2

echo ""
echo "================================================"
echo "  Coordinator + Client (zzttpi — Pi Zero W2)"
echo "    Client:     https://${ZZTTPI_IP}:${COORD_HTTP_PORT}  (self-signed TLS)"
echo "    Dashboard:  https://${ZZTTPI_IP}:${COORD_HTTP_PORT}/dashboard"
echo "    WebSocket:  ${ZZTTPI_IP}:${COORD_WS_PORT}"
echo ""
echo "  UNO Q boards (dashboard + sim-servers)"
for i in 1 2 3; do
  echo "    Board $i: http://${BOARD_IPS[$((i-1))]}:${BOARD_DASH_PORT}  (S$i: wss:$(sim_ws_port $i) udp:$(sim_udp_port $i) metrics:$(sim_metrics_port $i))"
done
echo ""
echo "  Assignment:  curl -k https://${ZZTTPI_IP}:${COORD_HTTP_PORT}/assign"
echo "  Metrics:"
for i in 1 2 3; do
  echo "    curl ${BOARD_IPS[$((i-1))]}:$(sim_metrics_port $i)/metrics"
done
echo ""
echo "  NOTE: Self-signed cert — accept the browser warning on first visit."
echo "        WebGPU requires secure context (HTTPS)."
echo "================================================"
echo ""
echo "Logs:"
echo "  Coordinator: ssh $ZZTTPI 'cat ~/mesh/coordinator.log'"
echo "  Board logs:  ssh $UNOQ 'cat ~/mesh/sim1.log'"
echo ""
echo "Test (cap steady-scale for boards):"
echo "  SERVER_URL=\$(curl -sk https://${ZZTTPI_IP}:${COORD_HTTP_PORT}/assign | python3 -c \"import sys,json; print(json.load(sys.stdin)['ws_url'])\")"
echo "  COORDINATOR_URL=https://${ZZTTPI_IP}:${COORD_HTTP_PORT} STEADY_SCALE_MAX=1500 METRICS_HOSTS=1=${UNOQ_IP},2=${UNOQ2_IP},3=${UNOQ3_IP} \\"
echo "    npx tsx tools/test-suite/runner.ts \$SERVER_URL $(sim_metrics_port 1) 3"
echo ""
echo "Stop: ./stop-unoq.sh"
