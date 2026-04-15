#!/usr/bin/env bash

DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/boards.env"

# Kill remote sim-servers (dashboard stays running as a persistent service)
echo "Stopping sim-servers on boards..."
ssh $UNOQ 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ2 'pkill -f sim-server' 2>/dev/null || true
ssh $UNOQ3 'pkill -f sim-server' 2>/dev/null || true

# Stop coordinator service on zzttpi
echo "Stopping coordinator on zzttpi..."
ssh $ZZTTPI 'systemctl --user stop mesh-coordinator' 2>/dev/null || true

echo "All stopped."
