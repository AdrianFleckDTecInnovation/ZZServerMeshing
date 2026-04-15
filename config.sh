#!/usr/bin/env bash
# Central configuration for the server meshing cluster.
# Source this from any script: source "$(dirname "$0")/config.sh"
#
# All port formulas and deployment defaults live here.
# Individual scripts can override values BEFORE sourcing this file.

# ── Coordinator ──
COORD_WS_PORT="${COORD_WS_PORT:-7870}"
COORD_HTTP_PORT="${COORD_HTTP_PORT:-7871}"

# ── Sim-Server port bases (actual port = base + server_id) ──
SIM_WS_BASE="${SIM_WS_BASE:-7877}"       # server 1 → 7878, server 2 → 7879, ...
SIM_UDP_BASE="${SIM_UDP_BASE:-9100}"      # server 1 → 9101, server 2 → 9102, ...
SIM_METRICS_BASE="${SIM_METRICS_BASE:-8100}" # server 1 → 8101, server 2 → 8102, ...

# ── Client ──
CLIENT_PORT="${CLIENT_PORT:-4321}"

# ── Board dashboard ──
BOARD_DASH_PORT="${BOARD_DASH_PORT:-3000}"

# ── Defaults ──
DEFAULT_NUM_SERVERS="${DEFAULT_NUM_SERVERS:-3}"
DEFAULT_TOTAL_NPCS="${DEFAULT_TOTAL_NPCS:-30}"    # local development
UNOQ_NPCS_PER_SERVER="${UNOQ_NPCS_PER_SERVER:-15}" # UNO Q boards (higher — boards handle it)
DOCKER_NPCS_PER_SERVER="${DOCKER_NPCS_PER_SERVER:-10}" # Docker (resource-constrained)

# ── Helper: compute sim-server ports for a given server_id ──
sim_ws_port()      { echo $(( SIM_WS_BASE + $1 )); }
sim_udp_port()     { echo $(( SIM_UDP_BASE + $1 )); }
sim_metrics_port() { echo $(( SIM_METRICS_BASE + $1 )); }
