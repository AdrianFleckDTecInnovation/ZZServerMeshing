#!/usr/bin/env bash
# Delete all Azure resources. This is destructive and irreversible.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/config.sh"

echo "This will DELETE the entire resource group: $RESOURCE_GROUP"
echo "  - AKS cluster ($AKS_NAME)"
echo "  - Container registry ($ACR_NAME)"
echo "  - App Service ($APP_NAME)"
echo ""
read -p "Type 'yes' to confirm: " CONFIRM
if [ "$CONFIRM" != "yes" ]; then
  echo "Aborted."
  exit 0
fi

echo ""
echo "=== Deleting resource group $RESOURCE_GROUP ==="
az group delete --name "$RESOURCE_GROUP" --yes --no-wait

echo ""
echo "Deletion started (runs in background, takes ~5 minutes)."
echo "Monitor: az group show --name $RESOURCE_GROUP -o table"
