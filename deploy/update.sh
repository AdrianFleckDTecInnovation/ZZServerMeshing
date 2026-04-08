#!/usr/bin/env bash
# Quick update: rebuild image, push, restart all pods.
# Usage:
#   ./deploy/update.sh           # update all services
#   ./deploy/update.sh --client  # also rebuild and deploy client
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

DEPLOY_CLIENT=false
for arg in "$@"; do
  case "$arg" in
    --client) DEPLOY_CLIENT=true ;;
  esac
done

echo "=== Rebuilding and pushing image ==="
az acr login --name "$ACR_NAME"
docker build -f "$ROOT/docker/Dockerfile.rust" -t "$IMAGE_NAME" "$ROOT"
docker push "$IMAGE_NAME"

echo ""
echo "=== Restarting pods ==="
kubectl rollout restart deployment/coordinator
kubectl rollout restart statefulset/sim-server
kubectl rollout restart deployment/gateway

echo "=== Waiting for rollout ==="
kubectl rollout status deployment/coordinator --timeout=120s
kubectl rollout status statefulset/sim-server --timeout=180s
kubectl rollout status deployment/gateway --timeout=120s

if [ "$DEPLOY_CLIENT" = true ]; then
  echo ""
  "$DIR/04-deploy-client.sh"
fi

echo ""
echo "=== Update complete ==="
kubectl get pods -o wide
