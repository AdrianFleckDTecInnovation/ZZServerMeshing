#!/bin/bash
# Deploy UNO Q dashboard to a board
# Usage: ./deploy.sh [unoq|unoq2]  (default: unoq)

set -e

BOARD="${1:-unoq}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STATIC_SRC="$SCRIPT_DIR/../static"
BINARY="$SCRIPT_DIR/target/aarch64-unknown-linux-gnu/release/unoq-dashboard"

# Build if needed
if [ ! -f "$BINARY" ] || [ "$1" = "--build" ]; then
    echo "Building for aarch64..."
    cd "$SCRIPT_DIR"
    cargo zigbuild --release --target aarch64-unknown-linux-gnu
fi

echo "Deploying dashboard to $BOARD..."

# Copy binary and static files
ssh "$BOARD" 'mkdir -p ~/dashboard/static'
scp "$BINARY" "$BOARD:~/dashboard/"
scp "$STATIC_SRC/pico.min.css" "$STATIC_SRC/htmx.min.js" "$STATIC_SRC/smoothie.min.js" \
    "$BOARD:~/dashboard/static/"

# Install user systemd service (no sudo needed)
ssh "$BOARD" 'mkdir -p ~/.config/systemd/user && cat > ~/.config/systemd/user/dashboard.service << EOF
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
EOF
systemctl --user daemon-reload
systemctl --user enable dashboard
systemctl --user restart dashboard
loginctl enable-linger arduino 2>/dev/null || true'

echo ""
echo "Dashboard deployed! Access at http://$BOARD.local:3000"
echo "Logs: ssh $BOARD 'journalctl --user -u dashboard -f'"
