#!/bin/sh
set -eu

: "${YAM_PKG:?YAM_PKG is required}"
: "${YAM_ROOT:?YAM_ROOT is required}"
: "${YAM_VAR:?YAM_VAR is required}"

data="$YAM_VAR/luna/data"
pidfile="$YAM_VAR/luna/luna.pid"
logfile="$YAM_VAR/luna/luna.log"
binary="$YAM_PKG/bin/luna"
stable_binary="$YAM_ROOT/bin/luna"
init="/etc/init.d/S99luna"
listen="${LUNA_LISTEN:-0.0.0.0:8099}"
yam_run="${YAM_RUN:-/tmp/yam}"

[ -x "$binary" ] || { echo "Luna binary is missing: $binary" >&2; exit 1; }
mkdir -p "$data"

if [ -e "$init" ] || [ -L "$init" ]; then
  [ ! -L "$init" ] || { echo "refusing unmanaged symlink: $init" >&2; exit 1; }
  grep -Fqx '# Managed by Luna YAM package' "$init" || {
    echo "refusing unmanaged init script: $init" >&2
    exit 1
  }
  # YAM updates run this hook without LUNA_LISTEN. Keep the address of the
  # existing boot hook (e.g. an owner switched it to 127.0.0.1:8099) instead
  # of silently reopening the API to the whole network.
  if [ -z "${LUNA_LISTEN:-}" ]; then
    previous=$(sed -n "s/^listen='\(.*\)'\$/\1/p" "$init" | head -n 1)
    case "$previous" in
      ''|*[!]A-Za-z0-9.:_[-]*) ;;
      *) listen="$previous" ;;
    esac
  fi
fi

remounted=0
if mount -o remount,rw / 2>/dev/null; then remounted=1; fi
restore_root() {
  if [ "$remounted" -eq 1 ]; then mount -o remount,ro / 2>/dev/null || true; fi
}
trap restore_root EXIT

if [ ! -d /etc/init.d ]; then
  echo "init directory is unavailable: /etc/init.d" >&2
  exit 1
fi

tmp_init="$init.tmp.$$"
cat >"$tmp_init" <<EOF
#!/bin/sh
# Managed by Luna YAM package
set -eu

root='$YAM_ROOT'
var='$YAM_VAR'
binary="\$root/bin/luna"
data="\$var/luna/data"
pidfile="\$var/luna/luna.pid"
logfile="\$var/luna/luna.log"
listen='$listen'
mount -o remount,rw /
ln -sf /system/vendor/quasar/libs/libasound.so.2 /lib/libasound.so.2

case "\${1:-start}" in
start)
  [ -x "\$binary" ] || exit 0
  mkdir -p "\$data" '$yam_run'
  touch '$yam_run/yam.lock'
  if [ -f "\$pidfile" ]; then
    pid=\$(cat "\$pidfile" 2>/dev/null || true)
    case "\$pid" in
      ''|*[!0-9]*) rm -f "\$pidfile" ;;
      *)
        if kill -0 "\$pid" 2>/dev/null &&
           tr '\0' ' ' <"/proc/\$pid/cmdline" 2>/dev/null | grep -q 'luna'; then
          exit 0
        fi
        rm -f "\$pidfile"
        ;;
    esac
  fi
  nohup "\$binary" --listen "\$listen" --data-dir "\$data" \
    --yam-root "\$root" --yam-run '$yam_run' \
    >"\$logfile" 2>&1 </dev/null &
  echo \$! >"\$pidfile"
  ;;
stop)
  if [ -f "\$pidfile" ]; then
    pid=\$(cat "\$pidfile" 2>/dev/null || true)
    case "\$pid" in ''|*[!0-9]*) ;; *) kill "\$pid" 2>/dev/null || true ;; esac
    rm -f "\$pidfile"
  fi
  ;;
restart)
  "\$0" stop
  "\$0" start
  ;;
*)
  echo "usage: \$0 {start|stop|restart}" >&2
  exit 2
  ;;
esac
EOF
chmod 0755 "$tmp_init"
mv "$tmp_init" "$init"

"$init" start
sleep 1

pid=$(cat "$pidfile" 2>/dev/null || true)
case "$pid" in
  ''|*[!0-9]*) cat "$logfile" 2>/dev/null || true; exit 1 ;;
esac
kill -0 "$pid" 2>/dev/null || { cat "$logfile" 2>/dev/null || true; exit 1; }

echo "Luna started from YAM package: $YAM_PKG"
echo "Luna boot hook installed: $init -> $stable_binary"
