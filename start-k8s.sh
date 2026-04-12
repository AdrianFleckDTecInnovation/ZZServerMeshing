#!/usr/bin/env bash
set -e

# Kubernetes Mesh Deployment (AKS)
# Builds container image, pushes to registry, deploys all services

DIR="$(cd "$(dirname "$0")" && pwd)"
REGISTRY="${REGISTRY:-}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
NAMESPACE="${NAMESPACE:-default}"
NUM_SERVERS="${1:-3}"

if [ -z "$REGISTRY" ]; then
  echo "Error: REGISTRY env var required (e.g. myregistry.azurecr.io/mesh)"
  echo ""
  echo "Usage:"
  echo "  REGISTRY=myregistry.azurecr.io/mesh ./start-k8s.sh [num-servers]"
  echo ""
  echo "Env vars:"
  echo "  REGISTRY     Container registry prefix (required)"
  echo "  IMAGE_TAG    Image tag (default: latest)"
  echo "  NAMESPACE    Kubernetes namespace (default: default)"
  exit 1
fi

IMAGE="${REGISTRY}:${IMAGE_TAG}"

echo "=== Building container image ==="
docker build -t "$IMAGE" -f docker/Dockerfile.rust "$DIR"

echo ""
echo "=== Pushing to registry ==="
docker push "$IMAGE"

echo ""
echo "=== Deploying to Kubernetes (namespace: $NAMESPACE) ==="

# Apply manifests with image substitution
for manifest in coordinator gateway sim-server; do
  echo "  Applying $manifest..."
  sed "s|IMAGE_PLACEHOLDER|${IMAGE}|g" "$DIR/k8s/${manifest}.yaml" \
    | kubectl apply -n "$NAMESPACE" -f -
done

# Scale sim-servers
if [ "$NUM_SERVERS" != "3" ]; then
  echo "  Scaling sim-servers to $NUM_SERVERS..."
  kubectl scale statefulset sim-server --replicas="$NUM_SERVERS" -n "$NAMESPACE"
fi

echo ""
echo "=== Waiting for rollout ==="
kubectl rollout status deployment/coordinator -n "$NAMESPACE" --timeout=120s
kubectl rollout status deployment/gateway -n "$NAMESPACE" --timeout=120s
kubectl rollout status statefulset/sim-server -n "$NAMESPACE" --timeout=120s

echo ""
echo "=== Getting gateway external IP ==="
GATEWAY_IP=""
for i in $(seq 1 30); do
  GATEWAY_IP=$(kubectl get svc gateway -n "$NAMESPACE" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null)
  if [ -n "$GATEWAY_IP" ]; then break; fi
  sleep 2
done

echo ""
echo "================================================"
echo "  Namespace: $NAMESPACE"
echo "  Image:     $IMAGE"
echo "  Replicas:  $NUM_SERVERS sim-servers"
echo ""
if [ -n "$GATEWAY_IP" ]; then
  echo "  Gateway:   ws://${GATEWAY_IP}"
  echo "  Client:    Set VITE_GATEWAY_URL=ws://${GATEWAY_IP} and run client locally"
else
  echo "  Gateway:   pending external IP (check: kubectl get svc gateway -n $NAMESPACE)"
fi
echo ""
echo "  Logs:"
echo "    kubectl logs -f deploy/coordinator -n $NAMESPACE"
echo "    kubectl logs -f statefulset/sim-server -n $NAMESPACE"
echo "    kubectl logs -f deploy/gateway -n $NAMESPACE"
echo "================================================"
echo ""
echo "Stop: ./stop-k8s.sh"
