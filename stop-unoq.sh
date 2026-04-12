#!/usr/bin/env bash

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids-unoq"

# Kill remote sim-servers (dashboard stays running as a persistent service)
echo "Stopping sim-servers on boards..."
ssh unoq 'pkill -f sim-server' 2>/dev/null || true
ssh unoq2 'pkill -f sim-server' 2>/dev/null || true

# Kill local processes
if [ -f "$PIDFILE" ]; then
  while read -r PID; do
    if kill -0 "$PID" 2>/dev/null; then
      kill "$PID" 2>/dev/null
      echo "Stopped PID $PID"
    fi
  done < "$PIDFILE"
  rm -f "$PIDFILE"
fi

echo "All stopped."
