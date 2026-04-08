#!/usr/bin/env bash

DIR="$(cd "$(dirname "$0")" && pwd)"
PIDFILE="$DIR/.pids"

if [ ! -f "$PIDFILE" ]; then
  echo "Nothing running (.pids not found)."
  exit 0
fi

while read -r PID; do
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null
    echo "Stopped PID $PID"
  else
    echo "PID $PID already gone"
  fi
done < "$PIDFILE"

rm -f "$PIDFILE"
echo "All stopped."
