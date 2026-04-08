#!/usr/bin/env bash
# Build Docker image and push to ACR.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
source "$DIR/config.sh"

echo "=== Logging into ACR ==="
az acr login --name "$ACR_NAME"

echo ""
echo "=== Building Docker image ==="
docker build -f "$ROOT/docker/Dockerfile.rust" -t "$IMAGE_NAME" "$ROOT"

echo ""
echo "=== Pushing to ACR ==="
docker push "$IMAGE_NAME"

echo ""
echo "=== Image pushed: $IMAGE_NAME ==="
echo "Next: ./deploy/03-deploy-k8s.sh"
