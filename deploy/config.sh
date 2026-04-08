#!/usr/bin/env bash
# Shared config for all deploy scripts

export RESOURCE_GROUP="zzmesh2"
export LOCATION="westus2"
export ACR_NAME="zzmeshacr"
export AKS_NAME="zzmesh-aks"
export APP_SERVICE_PLAN="zzmesh-plan"
export APP_NAME="zzmesh-client"
export IMAGE_TAG="latest"
export IMAGE_NAME="${ACR_NAME}.azurecr.io/mesh-rust:${IMAGE_TAG}"
