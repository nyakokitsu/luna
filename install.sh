#!/bin/sh
set -eu
log() { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*" >&2; }

VERSION=${LUNA_VERSION:-latest}
PLATFORM=${LUNA_PLATFORM:-}
UNINSTALL=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --platform|-p)
      [ "$#" -ge 2 ] || { echo "$1 requires a value" >&2; exit 2; }
      PLATFORM=$2; shift 2 ;;
    --version)
      [ "$#" -ge 2 ] || { echo "$1 requires a value" >&2; exit 2; }
      VERSION=$2; shift 2 ;;
    --uninstall)
      UNINSTALL=1; shift ;;
    --help|-h)
      echo "Usage: $0 [VERSION] [--platform DEVICE] [--uninstall]"; exit 0 ;;
    -*)
      echo "Unknown option: $1" >&2; exit 2 ;;
    *)
      if [ "$VERSION" = "latest" ] && [ -z "${LUNA_VERSION:-}" ]; then
        VERSION=$1
      elif [ -z "$PLATFORM" ]; then
        PLATFORM=$1
      else
        echo "Unexpected argument: $1" >&2; exit 2
      fi
      shift ;;
  esac
done
REPOSITORY=${LUNA_REPOSITORY:-https://github.com/nyakokitsu/luna}
ROOT=${LUNA_REMOTE_ROOT:-/data/quazar/luna}
SERIAL=${ADB_SERIAL:-}
case "$ROOT" in ''|*[!A-Za-z0-9_./-]*) echo "invalid remote root" >&2; exit 2 ;; esac

case "$VERSION" in
  latest) URL="$REPOSITORY/releases/latest/download" ;;
  v*) URL="$REPOSITORY/releases/download/$VERSION" ;;
  *) URL="$REPOSITORY/releases/download/v$VERSION" ;;
esac
for tool in adb; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 4; }; done
if [ "$UNINSTALL" -eq 0 ]; then
  command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 4; }
  if command -v curl >/dev/null 2>&1; then DOWNLOAD=curl; elif command -v wget >/dev/null 2>&1; then DOWNLOAD=wget; else echo "curl or wget is required" >&2; exit 4; fi
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/luna-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM
download() {
  if [ "$DOWNLOAD" = curl ]; then curl -fL --proto '=https,http' --tlsv1.2 "$1" -o "$2"; else wget -O "$2" "$1"; fi
}
verify() {
  expected=$(awk -v name="$(basename "$1")" '$2 == name {print $1}' "$TMP/SHA256SUMS")
  if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$1" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$1" | awk '{print $1}')
  else actual=$(openssl dgst -sha256 "$1" | awk '{print $NF}'); fi
  [ "$actual" = "$expected" ] || { echo "checksum mismatch: $1" >&2; exit 5; }
}
if [ -z "$SERIAL" ]; then
  devices=$(adb devices </dev/null | awk 'NR > 1 && $2 == "device" {print $1}')
  count=$(printf '%s\n' "$devices" | sed '/^\s*$/d' | wc -l | tr -d ' ')
  [ "$count" -gt 0 ] || { echo "No ADB station found." >&2; exit 6; }
  if [ "$count" -eq 1 ]; then SERIAL=$(printf '%s\n' "$devices" | head -n 1)
  else
    echo "$devices" | awk '{printf "%d) %s\n", NR, $1}' >&2
    printf "Station number: " >&2; read -r n < /dev/tty
    SERIAL=$(printf '%s\n' "$devices" | sed -n "${n}p")
  fi
fi
adb -s "$SERIAL" get-state </dev/null | grep -qx device || { echo "ADB station is offline" >&2; exit 6; }
log "Connected to station $SERIAL"

if [ "$UNINSTALL" -eq 1 ]; then
  REMOTE="$TMP/remote.sh"
  cat >"$REMOTE" <<'REMOTE'
set -eu
root="$LUNA_ROOT"; init=/etc/init.d/S99luna
[ -x "$init" ] && "$init" stop || true
rm -f "$init" "$root/bin/luna" "$root/bin/luna.previous" "$root/luna.pid"
echo "Luna removed"
REMOTE
  adb -s "$SERIAL" shell "LUNA_ROOT='$ROOT' sh -s" <"$REMOTE"
  log "Luna removed from $SERIAL"
  exit 0
fi

log "Downloading release metadata"
download "$URL/SHA256SUMS" "$TMP/SHA256SUMS"
download "$URL/manifest.json" "$TMP/manifest.json"
adb_optional() { adb -s "$SERIAL" shell "$1" </dev/null 2>/dev/null || true; }

detect_dt_platform() {
  dt_id=$(adb_optional 'cat /proc/device-tree/compatible /proc/device-tree/amlogic-dt-id 2>/dev/null | tr "\000" "\n"' | tr -d '\r \t' | tr '[:upper:]' '[:lower:]')
  case "$dt_id" in
    *yandex,station-mini2*|*station-mini2*|*axg_mini2*) printf '%s\n' yandexmini_2 ;;
    *yandex,cucumber*|*a5_cucumber*|*cucumber*) printf '%s\n' cucumber ;;
    *yandex,station-midi*) printf '%s\n' yandexstation_2 ;;
    *yandex,orion*|*a5_orion*|*orion*) printf '%s\n' orion ;;
    *yandex,pickle*|*pickle*|*amlogic,a4*) printf '%s\n' pickle ;;
    *yandex,bergamot*|*yandex,station-mini3*|*bergamot*) printf '%s\n' bergamot ;;
    *yandex,saturn*|*saturn*) printf '%s\n' saturn ;;
    *yandex,mango*|*mango*|*amlogic,a1*) printf '%s\n' mango ;;
    *yandex,plum*|*yandex,prune*|*plum*|*prune*|*station-lite2*) printf '%s\n' plum ;;
    *yandex,yandexmicro*|*yandex*station*micro*|*yandexmicro*|*blackthorn*) printf '%s\n' yandexmicro ;;
    *axg_s420*|*amlogic,axg*) printf '%s\n' yandexmini ;;
    *) return 1 ;;
  esac
}

if [ -n "$PLATFORM" ]; then
  log "Using explicitly provided platform: $PLATFORM"
  TARGET="$PLATFORM"
  EXPLICIT=1
else
  log "Detecting station"
  TARGET=$(detect_dt_platform || true)
  if [ -n "$TARGET" ]; then
    log "Auto-detected platform from device-tree: $TARGET"
  else
    TARGET=$(adb_optional 'uname -n' | tr -d '\r')
    log "Auto-detected hostname: $TARGET"
  fi
  EXPLICIT=0
fi

[ -n "$TARGET" ] || { echo "Could not detect platform or hostname" >&2; exit 7; }

RECORD=$(python3 - "$TMP/manifest.json" "$TARGET" "$EXPLICIT" <<'PY'
import json, re, sys
data=json.load(open(sys.argv[1], encoding="utf-8"))
target=sys.argv[2].strip()
explicit=sys.argv[3] == "1"

def compact(x):
    if not x: return ""
    x = re.sub(r"[A-Z]{4}$", "", str(x).strip())
    # Отбрасываем серийный суффикс вроде "_F6D0" или "-16AK", только если
    # в нём есть цифра. "-mini", "-lite", "-midi" и "-gen2" — части имени модели.
    x = re.sub(r"[-_](?!gen[0-9]$)(?=[a-z]*[0-9])[a-z0-9]{4}$", "", x.lower())
    return re.sub(r"[^a-z0-9]", "", x)

t_norm = compact(target)
devices = data.get("devices", [])
def aliases(entry):
    names = entry.get("models", []) + [entry.get("device", ""), entry.get("codename", ""), entry.get("yam_target", "")]
    return [re.sub(r"[^a-z0-9]", "", str(x).strip().lower()) for x in names if x]

# Точное совпадение имеет приоритет; подстрока — только запасной вариант
# автоматического определения.
found = [e for e in devices if t_norm in aliases(e)]
if not found and not explicit:
    found = [e for e in devices if any(t_norm in a or a in t_norm for a in aliases(e))]

if len(found) != 1: sys.exit(1)
e = found[0]
print("\t".join((e["device"], e["asset"], e.get("codename", ""))))
PY
) || { echo "Unsupported or ambiguous station: $TARGET" >&2; exit 7; }

IFS="$(printf '\t')" read -r DEVICE ASSET CODENAME <<EOF
$RECORD
EOF
echo "Selected $DEVICE${CODENAME:+ ($CODENAME)} for $TARGET" >&2
log "Downloading $ASSET"
download "$URL/$ASSET" "$TMP/$ASSET"
log "Verifying checksum"
verify "$TMP/$ASSET"

REMOTE="$TMP/remote.sh"
cat >"$REMOTE" <<'REMOTE'
set -eu
root="$LUNA_ROOT"; binary="$root/bin/luna"; staged="$root/bin/luna.new"; pidfile="$root/luna.pid"; logfile="$root/luna.log"; init=/etc/init.d/S99luna
mount -o remount,rw / 2>/dev/null || true
mount | grep ' on / ' | grep -q '(rw' || { echo "root filesystem is not writable" >&2; exit 1; }
mkdir -p "$root/bin" "$root/data"; chmod 0755 "$staged"
[ ! -f "$init" ] || grep -q '^# Managed by Luna$' "$init" || { echo "refusing unmanaged $init" >&2; exit 1; }
if [ -x "$init" ]; then "$init" stop || true; fi
[ -f "$binary" ] && cp "$binary" "$root/bin/luna.previous"
mv "$staged" "$binary"
cat >"$init" <<'INIT'
#!/bin/sh
# Managed by Luna
set -eu
root=/data/quazar/luna; binary=$root/bin/luna; pidfile=$root/luna.pid; logfile=$root/luna.log
case "${1:-start}" in
start)
  [ -x "$binary" ] || exit 0; mkdir -p "$root/data"
  if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then exit 0; fi
  rm -f "$pidfile"
  if command -v start-stop-daemon >/dev/null 2>&1; then
    start-stop-daemon --start --background --make-pidfile --pidfile "$pidfile" --startas "$binary" -- --listen 0.0.0.0:8099 --data-dir "$root/data"
  else
    nohup "$binary" --listen 0.0.0.0:8099 --data-dir "$root/data" >"$logfile" 2>&1 </dev/null & echo $! >"$pidfile"
  fi ;;
stop)
  [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true; rm -f "$pidfile" ;;
restart) "$0" stop; "$0" start ;;
*) exit 2 ;;
esac
INIT
chmod 0755 "$init"; "$init" start; sleep 1
pid=$(cat "$pidfile" 2>/dev/null || true); case "$pid" in ''|*[!0-9]*) cat "$logfile" 2>/dev/null || true; exit 1 ;; esac
kill -0 "$pid" 2>/dev/null || { cat "$logfile" 2>/dev/null || true; exit 1; }
REMOTE
log "Uploading $ASSET to $ROOT"
adb -s "$SERIAL" push "$TMP/$ASSET" "$ROOT/bin/luna.new" </dev/null >/dev/null
log "Starting Luna on the station"
adb -s "$SERIAL" shell "LUNA_ROOT='$ROOT' sh -s" <"$REMOTE"
log "Luna installed: $DEVICE ($CODENAME), API port 8099"
