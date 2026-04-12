#!/usr/bin/env bash

NAMESPACE="${NAMESPACE:-default}"

echo "Deleting mesh services from namespace: $NAMESPACE"

kubectl delete -f k8s/sim-server.yaml -n "$NAMESPACE" --ignore-not-found
kubectl delete -f k8s/gateway.yaml -n "$NAMESPACE" --ignore-not-found
kubectl delete -f k8s/coordinator.yaml -n "$NAMESPACE" --ignore-not-found

echo "All stopped."
