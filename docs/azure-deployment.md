# Azure Deployment Plan

> **Note:** This document predates the gateway removal and true peer-to-peer mesh migration. The gateway references below are outdated — clients now connect directly to sim-servers after coordinator assignment. The AKS manifests need updating to remove the gateway Deployment and add the coordinator's HTTP assignment endpoint. See [architecture.md](architecture.md) for the current architecture.

Deploy the server meshing prototype to Azure for real cross-host network testing. The client is hosted on **Azure App Service** (static files). Everything else runs on **AKS** with sim-servers spread across different nodes to test real UDP latency.

All services are Rust — coordinator and sim-server share one Docker image with different entrypoints.

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
         └──────────────┼─►│  Gateway (Rust) │──────┘   │
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
| Gateway (Rust) | AKS (LoadBalancer) | Yes — public IP | Browser WebSocket, binary protocol |
| Coordinator | AKS (ClusterIP) | No — internal only | Only sim-servers and gateway connect |
| Sim-Servers (N) | AKS (headless Service) | No — internal only | Communicate via UDP, no public access |

## Prerequisites

- Azure subscription with permissions to create AKS, ACR, App Service
- Azure CLI (`az`) installed locally
- `kubectl` installed locally
- Docker installed locally (for building images)

## Step 1: Create Azure Resources

```bash
# Variables (customize these)
RESOURCE_GROUP="rg-servermesh"
LOCATION="eastus"
ACR_NAME="servermeshacr"           # must be globally unique
AKS_NAME="aks-servermesh"
APP_SERVICE_PLAN="plan-servermesh"
APP_NAME="servermesh-client"       # must be globally unique

# Resource group
az group create --name $RESOURCE_GROUP --location $LOCATION

# Container registry
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

## Step 2: Build & Push Docker Image

One image for all three Rust services — `docker/Dockerfile.rust` builds coordinator, sim-server, and gateway binaries.

```bash
# Login to ACR
az acr login --name $ACR_NAME

# Build and push (single image for all Rust services)
docker build -f docker/Dockerfile.rust -t $ACR_NAME.azurecr.io/mesh-rust:latest .
docker push $ACR_NAME.azurecr.io/mesh-rust:latest
```

No separate gateway image needed — the Node.js gateway has been replaced with Rust.

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
              ORDINAL=${HOSTNAME##*-}
              SERVER_ID=$((ORDINAL + 1))
              WS_PORT=$((7877 + SERVER_ID))
              UDP_PORT=$((9100 + SERVER_ID))
              METRICS_PORT=$((8100 + SERVER_ID))
              exec sim-server \
                --server-id $SERVER_ID \
                --port $WS_PORT \
                --udp-port $UDP_PORT \
                --metrics-port $METRICS_PORT \
                --coordinator coordinator:7870 \
                --initial-npcs 10
          ports:
            - containerPort: 7878
              protocol: TCP
            - containerPort: 9101
              protocol: UDP
            - containerPort: 8101
              protocol: TCP
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
    - name: metrics
      port: 8101
      protocol: TCP
```

**Note:** The coordinator broadcasts pod IPs to each sim-server via `PeerList`. In AKS, K8s pod networking is flat — any pod can reach any other pod by IP, so UDP replication works out of the box. No `--advertise-host` needed (that's only for Docker Compose where container hostnames aren't IPs).

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
          image: ${ACR_NAME}.azurecr.io/mesh-rust:latest
          command:
            - gateway
            - "--port"
            - "4322"
            - "--coordinator"
            - "coordinator:7870"
            - "--metrics-port"
            - "8200"
          ports:
            - containerPort: 4322
            - containerPort: 8200
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
    - name: ws
      port: 80
      targetPort: 4322
    - name: metrics
      port: 8200
      targetPort: 8200
```

After applying, get the public IP:
```bash
kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

## Step 4: Client Code Change

The client currently hardcodes the gateway URL. Needs to be configurable for deployment.

**File:** `client/src/main.ts`

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

## Step 5: Deploy Client to App Service

```bash
cd client

# Build with gateway URL pointing to AKS LoadBalancer IP
VITE_GATEWAY_URL="ws://$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" \
  npm run build

# ZIP deploy
cd dist && zip -r ../client.zip . && cd ..
az webapp deploy \
  --resource-group $RESOURCE_GROUP \
  --name $APP_NAME \
  --src-path client.zip \
  --type zip
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
2. Build and push Docker image to ACR
3. Deploy AKS manifests (coordinator → sim-servers → gateway)
4. Wait for gateway LoadBalancer IP
5. Build client with `VITE_GATEWAY_URL` pointing to gateway IP
6. Deploy client `dist/` to App Service
7. Open `https://$APP_NAME.azurewebsites.net` in browser

## Running the Test Suite Against AKS

Port-forward the metrics endpoints, then run the test suite:

```bash
# In separate terminals, forward each sim-server's metrics port
kubectl port-forward sim-server-0 8101:8101 &
kubectl port-forward sim-server-1 8102:8102 &
kubectl port-forward sim-server-2 8103:8103 &
kubectl port-forward svc/gateway 8200:8200 &

# Get gateway public IP
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')

# Run test suite
cd tools
npx tsx test-suite/runner.ts "ws://${GW_IP}" 8101 3 --json
```

## Updating After Code Changes

```bash
# Rebuild and push (one image for all Rust services)
docker build -f docker/Dockerfile.rust -t $ACR_NAME.azurecr.io/mesh-rust:latest .
docker push $ACR_NAME.azurecr.io/mesh-rust:latest

# Restart all services
kubectl rollout restart deployment/coordinator
kubectl rollout restart statefulset/sim-server
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

# Metrics (with port-forward active)
curl localhost:8101/metrics | jq .   # sim-server 1
curl localhost:8200/metrics | jq .   # gateway

# Shell into a pod
kubectl exec -it sim-server-0 -- /bin/sh

# Check UDP connectivity between sim-servers
kubectl exec -it sim-server-0 -- sh -c 'nc -u -z sim-server-1.sim-server 9102'
```

## Protocol Notes

The gateway uses a **binary WebSocket protocol** for client broadcasts (not JSON). This reduces bandwidth ~5x:

| Protocol | Per-entity size | 5K entities/frame |
|----------|----------------|-------------------|
| JSON | ~200 bytes | ~1 MB |
| Binary | 43 bytes | ~215 KB |

Additionally, **delta compression** sends only changed entities most ticks, with a full snapshot every 1 second for reliability.

Frame types (first byte):
- `0x01` — Full snapshot (all entities)
- `0x02` — Delta (changed entities + removed entity IDs)

## Cost Estimate (Pay-As-You-Go)

| Resource | SKU | Approx. Monthly |
|----------|-----|-----------------|
| AKS control plane | Free | $0 |
| AKS nodes (3x) | Standard_D2s_v5 (2 vCPU, 8GB) | ~$210 |
| Container Registry | Basic | ~$5 |
| App Service | B1 (1 core, 1.75GB) | ~$13 |
| Load Balancer | Standard | ~$18 |
| **Total** | | **~$246/mo** |

Can reduce cost by using B-series burstable VMs for AKS nodes (~$90/mo for 3x B2ms).

## Open Questions

- [ ] Azure subscription and resource group naming
- [ ] Region selection (closest to testers for lowest latency)
- [ ] Custom domain / TLS for the client App Service?
- [ ] CI/CD pipeline (GitHub Actions?) or manual deploys?
- [ ] Node VM size — D2s_v5 (steady) vs B2ms (burstable, cheaper)?
