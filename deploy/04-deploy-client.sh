#!/usr/bin/env bash
# Build client with gateway URL and deploy to App Service.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

# Get gateway public IP
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
if [ -z "$GW_IP" ]; then
  echo "ERROR: Gateway LoadBalancer IP not found. Run 03-deploy-k8s.sh first."
  exit 1
fi

echo "=== Building client (gateway: ws://$GW_IP) ==="
cd "$ROOT/client"
npm install
VITE_GATEWAY_URL="ws://$GW_IP" npm run build

echo ""
echo "=== Deploying to App Service: $APP_NAME ==="
cd dist
zip -r "$ROOT/.client-deploy.zip" . -x '*.DS_Store'
cd "$ROOT"

az webapp deploy \
  --resource-group "$RESOURCE_GROUP" \
  --name "$APP_NAME" \
  --src-path .client-deploy.zip \
  --type zip

rm -f .client-deploy.zip

echo ""
echo "=== Client deployed ==="
echo "  URL: https://$APP_NAME.azurewebsites.net"
echo "  Gateway: ws://$GW_IP"
