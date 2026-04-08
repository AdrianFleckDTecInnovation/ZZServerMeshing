#!/usr/bin/env bash
# Deploy all services to AKS. Safe to re-run (updates in place).
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

echo "=== Ensuring kubectl context ==="
az aks get-credentials \
  --resource-group "$RESOURCE_GROUP" \
  --name "$AKS_NAME" \
  --overwrite-existing 2>/dev/null

echo ""
echo "=== Applying K8s manifests ==="

# Substitute image placeholder in manifests and apply
for manifest in coordinator.yaml sim-server.yaml gateway.yaml; do
  echo "  Applying $manifest..."
  sed "s|IMAGE_PLACEHOLDER|${IMAGE_NAME}|g" "$ROOT/k8s/$manifest" | kubectl apply -f -
done

echo ""
echo "=== Waiting for pods ==="
kubectl rollout status deployment/coordinator --timeout=120s
kubectl rollout status statefulset/sim-server --timeout=180s
kubectl rollout status deployment/gateway --timeout=120s

echo ""
echo "=== Waiting for gateway LoadBalancer IP ==="
for i in $(seq 1 30); do
  GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
  if [ -n "$GW_IP" ]; then
    break
  fi
  echo "  waiting... ($i/30)"
  sleep 5
done

if [ -z "$GW_IP" ]; then
  echo "ERROR: Gateway LoadBalancer IP not assigned within 150s"
  kubectl get svc gateway
  exit 1
fi

echo ""
echo "=== Cluster deployed ==="
kubectl get pods -o wide
echo ""
echo "  Gateway IP: $GW_IP"
echo "  Gateway WS: ws://$GW_IP"
echo "  Metrics:    http://$GW_IP:8200/metrics"
echo ""
echo "Next: ./deploy/04-deploy-client.sh"
