# Azure Deployment Plan

Deploy the server meshing prototype to Azure for real cross-host network testing. The client is hosted on **Azure App Service** (familiar workflow, static files). Everything else runs on **AKS** with sim-servers spread across different nodes to test real UDP latency.

## Architecture

```
                    Internet
                       │
          ┌────────────┼────────────┐
          │                         │
  Azure App Service            AKS Cluster
  ┌──────────────┐     ┌──────────────────────────────┐
  │    Client     │     │                              │
  │  (static JS)  │     │  ┌───────────┐  ClusterIP    │
  │  port 80/443  │     │  │Coordinator│◄──────────┐   │
  └──────┬───────┘     │  │  :7870    │           │   │
         │              │  └───────────┘           │   │
         │              │                          │   │
         │ ws://        │  ┌─────────┐ ┌─────────┐ │   │
         │              │  │  Sim 1  │ │  Sim 2  │ │   │
         │              │  │ ws:7878 │ │ ws:7879 │ │   │
         │              │  │udp:9101 │ │udp:9102 │ │   │
         │              │  └────┬────┘ └────┬────┘ │   │
         │              │       │  UDP ◄────┘      │   │
         │              │       │                   │   │
         │              │  ┌─────────┐              │   │
         │              │  │  Sim 3  │              │   │
         │              │  │ ws:7880 │              │   │
         │              │  │udp:9103 │              │   │
         │              │  └─────────┘              │   │
         │              │                           │   │
         │              │  ┌─────────────────┐      │   │
         └──────────────┼─►│    Gateway      │──────┘   │
                        │  │    :4322        │          │
                        │  │  LoadBalancer   │          │
                        │  │  (public IP)    │          │
                        │  └─────────────────┘          │
                        └──────────────────────────────┘
```

## What Goes Where

| Service | Hosting | Exposed? | Why |
|---------|---------|----------|-----|
| Client (Vite build) | App Service | Yes — public URL | Static files, familiar deploy workflow |
| Gateway | AKS (LoadBalancer) | Yes — public IP | Browser needs WebSocket access |
| Coordinator | AKS (ClusterIP) | No — internal only | Only sim-servers and gateway connect |
| Sim-Servers (N) | AKS (headless Service) | No — internal only | Communicate via UDP, no public access |

## Prerequisites

- Azure subscription with permissions to create AKS, ACR, App Service
- Azure CLI (`az`) installed locally
- `kubectl` installed locally
- Docker installed locally (for building images)

## Step 1: Create Azure Resources

Names and regions are placeholders — fill in during setup.

```bash
# Variables (customize these)
RESOURCE_GROUP="rg-servermesh"
LOCATION="eastus"                  # pick region closest to testers
ACR_NAME="servermeshacr"           # must be globally unique
AKS_NAME="aks-servermesh"
APP_SERVICE_PLAN="plan-servermesh"
APP_NAME="servermesh-client"       # must be globally unique

# Resource group
az group create --name $RESOURCE_GROUP --location $LOCATION

# Container registry (for Docker images)
az acr create \
  --resource-group $RESOURCE_GROUP \
  --name $ACR_NAME \
  --sku Basic

# AKS cluster — 3 nodes so sim-servers land on different hosts
az aks create \
  --resource-group $RESOURCE_GROUP \
  --name $AKS_NAME \
  --node-count 3 \
  --node-vm-size Standard_D2s_v5 \
  --attach-acr $ACR_NAME \
  --generate-ssh-keys

# Get kubectl credentials
az aks get-credentials --resource-group $RESOURCE_GROUP --name $AKS_NAME

# App Service plan + web app (for client static files)
az appservice plan create \
  --name $APP_SERVICE_PLAN \
  --resource-group $RESOURCE_GROUP \
  --sku B1 \
  --is-linux

az webapp create \
  --resource-group $RESOURCE_GROUP \
  --plan $APP_SERVICE_PLAN \
  --name $APP_NAME \
  --runtime "NODE:20-lts"
```

## Step 2: Dockerfiles

Three images needed: Rust services (shared image, different entrypoint), gateway, and client (build-only, for local iteration).

### `docker/Dockerfile.rust`

Multi-stage build for both coordinator and sim-server binaries.

```dockerfile
# Build stage
FROM rust:1.83-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p coordinator -p sim-server

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/coordinator /usr/local/bin/
COPY --from=builder /app/target/release/sim-server /usr/local/bin/
```

### `docker/Dockerfile.gateway`

```dockerfile
FROM node:22-alpine
WORKDIR /app
COPY gateway/package.json gateway/package-lock.json ./
RUN npm ci
COPY gateway/ .
CMD ["npx", "tsx", "src/index.ts"]
```

### Build & push

```bash
# Login to ACR
az acr login --name $ACR_NAME

# Build and push Rust image
docker build -f docker/Dockerfile.rust -t $ACR_NAME.azurecr.io/mesh-rust:latest .
docker push $ACR_NAME.azurecr.io/mesh-rust:latest

# Build and push gateway image
docker build -f docker/Dockerfile.gateway -t $ACR_NAME.azurecr.io/mesh-gateway:latest .
docker push $ACR_NAME.azurecr.io/mesh-gateway:latest
```

## Step 3: Kubernetes Manifests

### `k8s/coordinator.yaml`

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: coordinator
spec:
  replicas: 1
  selector:
    matchLabels:
      app: coordinator
  template:
    metadata:
      labels:
        app: coordinator
    spec:
      containers:
        - name: coordinator
          image: ${ACR_NAME}.azurecr.io/mesh-rust:latest
          command: ["coordinator", "--port", "7870"]
          ports:
            - containerPort: 7870
---
apiVersion: v1
kind: Service
metadata:
  name: coordinator
spec:
  type: ClusterIP
  selector:
    app: coordinator
  ports:
    - port: 7870
      targetPort: 7870
```

### `k8s/sim-server.yaml`

StatefulSet gives each pod a stable hostname (`sim-server-0`, `sim-server-1`, etc.) and ordinal index for deriving server-id.

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: sim-server
spec:
  serviceName: sim-server
  replicas: 3
  selector:
    matchLabels:
      app: sim-server
  template:
    metadata:
      labels:
        app: sim-server
    spec:
      # Force each sim-server onto a different node
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - labelSelector:
                matchLabels:
                  app: sim-server
              topologyKey: kubernetes.io/hostname
      containers:
        - name: sim-server
          image: ${ACR_NAME}.azurecr.io/mesh-rust:latest
          command: ["/bin/sh", "-c"]
          args:
            - |
              # Derive server-id from pod ordinal (sim-server-0 → 1, sim-server-1 → 2, etc.)
              ORDINAL=${HOSTNAME##*-}
              SERVER_ID=$((ORDINAL + 1))
              WS_PORT=$((7877 + SERVER_ID))
              UDP_PORT=$((9100 + SERVER_ID))
              NPC_COUNT=10
              exec sim-server \
                --server-id $SERVER_ID \
                --port $WS_PORT \
                --udp-port $UDP_PORT \
                --coordinator coordinator:7870 \
                --initial-npcs $NPC_COUNT
          ports:
            - containerPort: 7878
              protocol: TCP
            - containerPort: 9101
              protocol: UDP
---
# Headless service for pod-to-pod DNS and UDP discovery
apiVersion: v1
kind: Service
metadata:
  name: sim-server
spec:
  type: ClusterIP
  clusterIP: None  # headless
  selector:
    app: sim-server
  ports:
    - name: ws
      port: 7878
      protocol: TCP
    - name: udp
      port: 9101
      protocol: UDP
```

**Important:** The coordinator broadcasts peer IPs to each sim-server via `PeerList`. In AKS, these will be pod IPs. The sim-servers use these IPs directly for UDP replication — this works out of the box because K8s pod networking is flat (any pod can reach any other pod by IP).

### `k8s/gateway.yaml`

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: gateway
spec:
  replicas: 1
  selector:
    matchLabels:
      app: gateway
  template:
    metadata:
      labels:
        app: gateway
    spec:
      containers:
        - name: gateway
          image: ${ACR_NAME}.azurecr.io/mesh-gateway:latest
          env:
            - name: COORDINATOR_URL
              value: "ws://coordinator:7870"
            - name: GATEWAY_PORT
              value: "4322"
          ports:
            - containerPort: 4322
---
apiVersion: v1
kind: Service
metadata:
  name: gateway
spec:
  type: LoadBalancer
  selector:
    app: gateway
  ports:
    - port: 80
      targetPort: 4322
```

After applying, get the public IP:
```bash
kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

## Step 4: Client Code Change

The client currently hardcodes the gateway URL. Needs to be configurable for deployment.

**File:** `client/src/main.ts`, line 8

```typescript
// Before (hardcoded)
const network = new NetworkClient('ws://localhost:4322', worldView);

// After (configurable via Vite env)
const gatewayUrl = import.meta.env.VITE_GATEWAY_URL || 'ws://localhost:4322';
const network = new NetworkClient(gatewayUrl, worldView);
```

When building for Azure, set the env var to the gateway's public IP:

```bash
cd client
VITE_GATEWAY_URL="ws://<gateway-public-ip>" npm run build
```

This produces a `dist/` folder of static files.

## Step 5: Deploy Client to App Service

After building with the correct gateway URL:

```bash
cd client

# Build with gateway URL pointing to AKS LoadBalancer IP
VITE_GATEWAY_URL="ws://$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" \
  npm run build

# Deploy static files to App Service
# Option A: ZIP deploy
cd dist && zip -r ../client.zip . && cd ..
az webapp deploy \
  --resource-group $RESOURCE_GROUP \
  --name $APP_NAME \
  --src-path client.zip \
  --type zip

# Option B: Use App Service local git deploy or GitHub Actions
```

The client will be available at: `https://$APP_NAME.azurewebsites.net`

## Step 6: Deploy to AKS

```bash
# Apply all manifests
kubectl apply -f k8s/coordinator.yaml
kubectl apply -f k8s/sim-server.yaml
kubectl apply -f k8s/gateway.yaml

# Wait for pods
kubectl get pods -w

# Check sim-servers are on different nodes
kubectl get pods -o wide

# Get gateway public IP (may take 1-2 minutes)
kubectl get svc gateway -w
```

## Deployment Order

1. Create Azure resources (resource group, ACR, AKS, App Service)
2. Build and push Docker images to ACR
3. Deploy AKS manifests (coordinator first, then sim-servers, then gateway)
4. Wait for gateway LoadBalancer IP
5. Build client with `VITE_GATEWAY_URL` pointing to gateway IP
6. Deploy client `dist/` to App Service
7. Open `https://$APP_NAME.azurewebsites.net` in browser

## Updating After Code Changes

```bash
# Rust changes: rebuild and push image, then restart
docker build -f docker/Dockerfile.rust -t $ACR_NAME.azurecr.io/mesh-rust:latest .
docker push $ACR_NAME.azurecr.io/mesh-rust:latest
kubectl rollout restart statefulset/sim-server
kubectl rollout restart deployment/coordinator

# Gateway changes: rebuild and push, restart
docker build -f docker/Dockerfile.gateway -t $ACR_NAME.azurecr.io/mesh-gateway:latest .
docker push $ACR_NAME.azurecr.io/mesh-gateway:latest
kubectl rollout restart deployment/gateway

# Client changes: rebuild and redeploy to App Service
VITE_GATEWAY_URL="ws://$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" \
  npm run build
cd dist && zip -r ../client.zip . && cd ..
az webapp deploy --resource-group $RESOURCE_GROUP --name $APP_NAME --src-path client.zip --type zip
```

## Scaling Sim-Servers

```bash
# Scale to 5 sim-servers (may need more AKS nodes)
kubectl scale statefulset sim-server --replicas=5

# Add nodes to the cluster if needed
az aks scale --resource-group $RESOURCE_GROUP --name $AKS_NAME --node-count 5
```

## Monitoring & Debugging

```bash
# Logs
kubectl logs -f deployment/coordinator
kubectl logs -f statefulset/sim-server        # follows pod 0
kubectl logs -f sim-server-1                   # specific server
kubectl logs -f deployment/gateway

# Shell into a pod
kubectl exec -it sim-server-0 -- /bin/sh

# Check UDP connectivity between sim-servers
kubectl exec -it sim-server-0 -- sh -c 'apt-get update && apt-get install -y netcat-openbsd && nc -u -z sim-server-1.sim-server 9102'
```

## Cost Estimate (Pay-As-You-Go)

| Resource | SKU | Approx. Monthly |
|----------|-----|-----------------|
| AKS control plane | Free | $0 |
| AKS nodes (3x) | Standard_D2s_v5 (2 vCPU, 8GB) | ~$210 |
| Container Registry | Basic | ~$5 |
| App Service | B1 (1 core, 1.75GB) | ~$13 |
| Load Balancer | Standard | ~$18 |
| **Total** | | **~$246/mo** |

Can reduce cost by using B-series burstable VMs for AKS nodes (~$90/mo for 3x B2ms) if sustained compute isn't needed.

## Open Questions

- [ ] Azure subscription and resource group naming
- [ ] Region selection (closest to testers for lowest latency to client/gateway)
- [ ] Custom domain / TLS for the client App Service?
- [ ] CI/CD pipeline (GitHub Actions?) or manual deploys for now?
- [ ] How many sim-servers for initial testing?
- [ ] Node VM size — D2s_v5 (steady) vs B2ms (burstable, cheaper)?
