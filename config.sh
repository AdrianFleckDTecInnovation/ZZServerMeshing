#!/usr/bin/env bash
# Central configuration for the server meshing cluster.
# Source this from any script: source "$(dirname "$0")/config.sh"
#
# Local defaults are shared with the Node launcher. Board settings and port formulas are below.
# Individual scripts can override values BEFORE sourcing this file.

# The native Node launcher and Bash deployments use the same defaults.
while IFS='=' read -r mesh_key mesh_default || [ -n "$mesh_key" ]; do
  printf -v "$mesh_key" '%s' "${!mesh_key:-$mesh_default}"
done < "$(dirname "${BASH_SOURCE[0]}")/scripts/local-defaults.env"
unset mesh_key mesh_default

# ── Board dashboard ──
BOARD_DASH_PORT="${BOARD_DASH_PORT:-3000}"

# ── Defaults ──
UNOQ_NPCS_PER_SERVER="${UNOQ_NPCS_PER_SERVER:-15}" # UNO Q boards (higher — boards handle it)
DOCKER_NPCS_PER_SERVER="${DOCKER_NPCS_PER_SERVER:-10}" # Docker (resource-constrained)

# ── Helper: compute sim-server ports for a given server_id ──
sim_ws_port()      { echo $(( SIM_WS_BASE + $1 )); }
sim_udp_port()     { echo $(( SIM_UDP_BASE + $1 )); }
sim_metrics_port() { echo $(( SIM_METRICS_BASE + $1 )); }
