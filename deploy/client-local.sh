#!/usr/bin/env bash
# Run the client locally, pointing at the AKS gateway.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

# Get gateway public IP
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
if [ -z "$GW_IP" ]; then
  echo "ERROR: Gateway LoadBalancer IP not found. Is the AKS cluster running?"
  exit 1
fi

echo "=== Starting client against AKS gateway ==="
echo "  Gateway: ws://$GW_IP"
echo "  Client:  http://localhost:4321"
echo ""

cd "$ROOT/client"
npm install --silent 2>/dev/null
VITE_GATEWAY_URL="ws://$GW_IP" npm run dev
