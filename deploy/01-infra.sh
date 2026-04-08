#!/usr/bin/env bash
# Create all Azure resources from scratch.
# Run once. Idempotent — safe to re-run.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/config.sh"

echo "=== Creating resource group: $RESOURCE_GROUP ==="
az group create --name "$RESOURCE_GROUP" --location "$LOCATION" -o table

echo ""
echo "=== Creating container registry: $ACR_NAME ==="
az acr create \
  --resource-group "$RESOURCE_GROUP" \
  --name "$ACR_NAME" \
  --sku Basic \
  -o table

echo ""
echo "=== Creating AKS cluster: $AKS_NAME (3 nodes) ==="
az aks create \
  --resource-group "$RESOURCE_GROUP" \
  --name "$AKS_NAME" \
  --node-count 3 \
  --node-vm-size Standard_D2s_v5 \
  --attach-acr "$ACR_NAME" \
  --generate-ssh-keys \
  -o table

echo ""
echo "=== Getting kubectl credentials ==="
az aks get-credentials \
  --resource-group "$RESOURCE_GROUP" \
  --name "$AKS_NAME" \
  --overwrite-existing

echo ""
echo "=== Creating App Service plan ==="
az appservice plan create \
  --name "$APP_SERVICE_PLAN" \
  --resource-group "$RESOURCE_GROUP" \
  --sku B1 \
  --is-linux \
  -o table

echo ""
echo "=== Creating web app: $APP_NAME ==="
az webapp create \
  --resource-group "$RESOURCE_GROUP" \
  --plan "$APP_SERVICE_PLAN" \
  --name "$APP_NAME" \
  --runtime "NODE:20-lts" \
  -o table

echo ""
echo "=== Infrastructure ready ==="
echo "  Resource group:  $RESOURCE_GROUP"
echo "  ACR:             $ACR_NAME.azurecr.io"
echo "  AKS:             $AKS_NAME (3x B2ms nodes)"
echo "  App Service:     $APP_NAME.azurewebsites.net"
echo ""
echo "Next: ./deploy/02-build-push.sh"
