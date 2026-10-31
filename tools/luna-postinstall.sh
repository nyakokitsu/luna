#!/bin/sh
set -eu

: "${YAM_PKG:?YAM_PKG is required}"
: "${YAM_VAR:?YAM_VAR is required}"

data="$YAM_VAR/luna/data"
pidfile="$YAM_VAR/luna/luna.pid"
logfile="$YAM_VAR/luna/luna.log"
binary="$YAM_PKG/bin/luna"
listen="${LUNA_LISTEN:-0.0.0.0:8099}"

[ -x "$binary" ] || { echo "Luna binary is missing: $binary" >&2; exit 1; }
mkdir -p "$data"

if [ -f "$pidfile" ]; then
  pid=$(cat "$pidfile" 2>/dev/null || true)
  case "$pid" in
    ''|*[!0-9]*) ;;
    *) kill "$pid" 2>/dev/null || true; rm -f "$pidfile"; sleep 1 ;;
  esac
fi

nohup "$binary" \
  --listen "$listen" \
  --data-dir "$data" \
  >"$logfile" 2>&1 </dev/null &
echo $! >"$pidfile"
sleep 1

pid=$(cat "$pidfile" 2>/dev/null || true)
case "$pid" in
  ''|*[!0-9]*) cat "$logfile" 2>/dev/null || true; exit 1 ;;
esac
kill -0 "$pid" 2>/dev/null || { cat "$logfile" 2>/dev/null || true; exit 1; }

echo "Luna started from YAM package: $YAM_PKG"
