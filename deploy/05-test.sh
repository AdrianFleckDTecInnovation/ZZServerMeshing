#!/usr/bin/env bash
# Run the test suite against the AKS cluster.
# Port-forwards metrics endpoints and runs scenarios.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

SCENARIOS="${1:-spawn-burst}"

# Get gateway public IP
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
if [ -z "$GW_IP" ]; then
  echo "ERROR: Gateway LoadBalancer IP not found."
  exit 1
fi

echo "=== Port-forwarding metrics endpoints ==="
# Kill any existing port-forwards
pkill -f "kubectl port-forward.*810" 2>/dev/null || true
sleep 1

kubectl port-forward sim-server-0 8101:8101 &
PF1=$!
kubectl port-forward sim-server-1 8102:8102 &
PF2=$!
kubectl port-forward sim-server-2 8103:8103 &
PF3=$!
kubectl port-forward svc/gateway 8200:8200 &
PF4=$!

cleanup() {
  kill $PF1 $PF2 $PF3 $PF4 2>/dev/null || true
}
trap cleanup EXIT INT TERM

sleep 3

echo ""
echo "=== Running test suite against ws://$GW_IP ==="
cd "$ROOT/tools"
npx tsx test-suite/runner.ts "ws://${GW_IP}" 8101 3 --no-color --json "$SCENARIOS"
