# Hardware Stack

The server mesh runs entirely on embedded hardware — no Mac or cloud required. Three Arduino UNO Q boards run the game servers, and a Raspberry Pi Zero W2 runs the coordinator and serves the client app.

## Boards

### Arduino UNO Q (x3)

The UNO Q is not a traditional Arduino microcontroller. It's a quad-core ARM single-board computer that runs full Linux. Each board runs one sim-server instance.

| Spec | Value |
|------|-------|
| CPU | 4-core ARM Cortex-A (aarch64) |
| RAM | 3.6 GB |
| OS | Debian Trixie 13 |
| User | `arduino` |
| Rust target | `aarch64-unknown-linux-gnu` |

### Raspberry Pi Zero W2 (zzttpi)

The Pi runs the coordinator and serves the built client app over HTTPS. It's the weakest device in the mesh — limited to 425 MB RAM — but the coordinator is lightweight.

| Spec | Value |
|------|-------|
| CPU | 4-core ARM Cortex-A53 (armv7l) |
| RAM | 425 MB |
| Connectivity | WiFi (2.4 GHz) |
| OS | Raspbian Trixie 13 |
| User | `dz` |
| Rust target | `armv7-unknown-linux-gnueabihf` |

## Network

All devices are on the same LAN with static IPs assigned via DHCP reservation on the router.

| Hostname | IP | mDNS |
|----------|----|------|
| zzttpi | 192.168.0.208 | zzttpi.local |
| unoq | 192.168.0.15 | unoq.local |
| unoq2 | 192.168.0.173 | unoq2.local |
| unoq3 | 192.168.0.86 | unoq3.local |

All connections between boards use the LAN. The Pi communicates with UNO Q boards over WebSocket (coordinator protocol) and the UNO Q boards replicate to each other over UDP. Browser clients connect from any device on the LAN.

## SSH Setup

Passwordless SSH from the Mac to all boards using a shared key. Configured in `~/.ssh/config`:

```
Host zzttpi
    HostName zzttpi.local
    User dz
    IdentityFile ~/.ssh/pi_key

Host unoq
    HostName unoq.local
    User arduino
    IdentityFile ~/.ssh/pi_key

Host unoq2
    HostName unoq2.local
    User arduino
    IdentityFile ~/.ssh/pi_key

Host unoq3
    HostName unoq3.local
    User arduino
    IdentityFile ~/.ssh/pi_key
```

The public key (`~/.ssh/pi_key.pub`) is in `~/.ssh/authorized_keys` on each board.

### First-time key setup

```sh
# Generate key (if needed)
ssh-keygen -t ed25519 -f ~/.ssh/pi_key -N ""

# Copy to each board (will prompt for password once)
ssh-copy-id -i ~/.ssh/pi_key arduino@unoq.local
ssh-copy-id -i ~/.ssh/pi_key arduino@unoq2.local
ssh-copy-id -i ~/.ssh/pi_key arduino@unoq3.local
ssh-copy-id -i ~/.ssh/pi_key dz@zzttpi.local
```

## Systemd Services

Both the coordinator and board dashboards run as user-level systemd services with linger enabled (start on boot without login).

### Coordinator (`zzttpi`)

Service file at `~/.config/systemd/user/mesh-coordinator.service`:

```ini
[Unit]
Description=Mesh Coordinator

[Service]
Type=simple
WorkingDirectory=/home/dz/mesh
ExecStart=/home/dz/mesh/coordinator \
    --port 7870 --http-port 7871 \
    --static-dir /home/dz/mesh/client-dist \
    --tls-cert /home/dz/mesh/certs/mesh-coordinator.crt \
    --tls-key /home/dz/mesh/certs/mesh-coordinator.key
Restart=always
RestartSec=3
StandardOutput=append:/home/dz/mesh/coordinator.log
StandardError=append:/home/dz/mesh/coordinator.log

[Install]
WantedBy=default.target
```

Install and enable:

```sh
ssh zzttpi 'mkdir -p ~/.config/systemd/user'
# (copy service file to ~/.config/systemd/user/mesh-coordinator.service)
ssh zzttpi 'systemctl --user daemon-reload && systemctl --user enable mesh-coordinator'
ssh zzttpi 'loginctl enable-linger dz'
```

### Board Dashboard (`unoq`, `unoq2`, `unoq3`)

Service file at `~/.config/systemd/user/dashboard.service`:

```ini
[Unit]
Description=UNO Q System Dashboard

[Service]
Type=simple
WorkingDirectory=/home/arduino/dashboard
ExecStart=/home/arduino/dashboard/unoq-dashboard
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
```

Installed automatically by `crates/board-dash/deploy.sh` or by `start-unoq.sh`. The deployment script also creates a systemd override at `~/.config/systemd/user/dashboard.service.d/mesh.conf` with environment variables that tell the dashboard how to start/stop the local sim-server:

```ini
[Service]
Environment=MESH_BINARY=/home/arduino/mesh/sim-server
Environment=MESH_ARGS=--server-id 1 --port 7878 --udp-port 9101 ...
```

## Directory Layout

### UNO Q boards

```
/home/arduino/
├── mesh/
│   ├── sim-server              # sim-server binary
│   ├── sim1.log                # (sim2.log on unoq2, sim3.log on unoq3)
│   └── certs/
│       ├── mesh-coordinator.crt
│       └── mesh-coordinator.key
├── dashboard/
│   ├── unoq-dashboard          # board-dash binary
│   └── static/
│       ├── pico.min.css
│       ├── htmx.min.js
│       └── smoothie.min.js
└── .config/systemd/user/
    ├── dashboard.service
    └── dashboard.service.d/
        └── mesh.conf           # sim-server env vars (set by start-unoq.sh)
```

### zzttpi (Pi Zero W2)

```
/home/dz/
├── mesh/
│   ├── coordinator             # coordinator binary
│   ├── coordinator.log
│   ├── client-dist/            # built Vite client app
│   │   ├── index.html
│   │   └── assets/
│   └── certs/
│       ├── mesh-coordinator.crt
│       └── mesh-coordinator.key
└── .config/systemd/user/
    └── mesh-coordinator.service
```

## Operational Commands

```sh
# Coordinator
ssh zzttpi 'systemctl --user status mesh-coordinator'
ssh zzttpi 'systemctl --user restart mesh-coordinator'
ssh zzttpi 'journalctl --user -u mesh-coordinator -f'
ssh zzttpi 'cat ~/mesh/coordinator.log'

# Sim-servers
ssh unoq 'pgrep -a sim-server'
ssh unoq 'cat ~/mesh/sim1.log'
ssh unoq 'pkill -f sim-server'

# Board dashboards
ssh unoq 'systemctl --user status dashboard'
ssh unoq 'journalctl --user -u dashboard -f'

# System health
ssh unoq 'free -h'
ssh unoq 'uptime'
ssh unoq 'cat /sys/class/thermal/thermal_zone*/temp'   # millidegrees C
```

## Adding a New Board

1. Set up the board with Debian, create user, configure static IP / DHCP reservation
2. Copy SSH key: `ssh-copy-id -i ~/.ssh/pi_key <user>@<hostname>.local`
3. Add SSH config entry in `~/.ssh/config`
4. Add IP/hostname to `start-unoq.sh` and `stop-unoq.sh`
5. Regenerate the TLS cert to include the new IP: `rm certs/mesh-coordinator.*` then run `./start-unoq.sh`
6. Re-trust the cert on the Mac (see [UNO Q Deployment](uno-q-deployment.md#tls))
