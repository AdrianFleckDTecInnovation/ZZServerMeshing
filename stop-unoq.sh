#!/usr/bin/env bash

# Kill remote sim-servers (dashboard stays running as a persistent service)
echo "Stopping sim-servers on boards..."
ssh unoq 'pkill -f sim-server' 2>/dev/null || true
ssh unoq2 'pkill -f sim-server' 2>/dev/null || true
ssh unoq3 'pkill -f sim-server' 2>/dev/null || true

# Stop coordinator service on zzttpi
echo "Stopping coordinator on zzttpi..."
ssh zzttpi 'systemctl --user stop mesh-coordinator' 2>/dev/null || true

echo "All stopped."
