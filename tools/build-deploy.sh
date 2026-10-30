#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
MANIFEST="$ROOT/luna-manifest.json"
DEVICE_ID="${LUNA_DEVICE:-}"
ADB_SERIAL="${ADB_SERIAL:-}"
REMOTE_ROOT="${LUNA_REMOTE_ROOT:-/data/quazar/luna}"
LISTEN="${LUNA_LISTEN:-0.0.0.0:8100}"

usage() {
  cat <<'USAGE'
Build and deploy one Luna station binary to an ADB-connected station.

Usage:
  tools/build-deploy.sh --device DEVICE_ID [--serial ADB_SERIAL]
  tools/build-deploy.sh DEVICE_ID [ADB_SERIAL]

Options:
  --device ID       Station entry from luna-manifest.json
  --serial SERIAL   ADB serial; required when several devices are connected
  --root PATH       Remote Luna root (default: /data/quazar/luna)
  --listen IP:PORT  Remote listen address (default: 0.0.0.0:8100)
USAGE
}

arrow_select() {
  local title=$1
  shift
  local items=("$@")
  local selected=0
  local tty=/dev/tty
  local saved
  saved=$(stty -g < "$tty")

  restore_terminal() {
    stty "$saved" < "$tty"
  }
  trap restore_terminal EXIT INT TERM
  stty -icanon -echo min 1 time 0 < "$tty"

  while :; do
    printf '\033[2J\033[H' > "$tty"
    printf '%s\n\n' "$title" > "$tty"
    local index
    for index in "${!items[@]}"; do
      if ((index == selected)); then
        printf '  \033[36m> %s\033[0m\n' "${items[index]}" > "$tty"
      else
        printf '    %s\n' "${items[index]}" > "$tty"
      fi
    done
    printf '\n↑/↓ выбрать, Enter подтвердить, q отмена\n' > "$tty"

    local key sequence
    IFS= read -r -n 1 key < "$tty"
    case "$key" in
      $'\x1b')
        IFS= read -r -n 2 sequence < "$tty"
        case "$sequence" in
          '[A') ((selected = (selected - 1 + ${#items[@]}) % ${#items[@]})) ;;
          '[B') ((selected = (selected + 1) % ${#items[@]})) ;;
        esac
        ;;
      '')
        printf '\033[2J\033[H' > "$tty"
        printf -v REPLY '%s' "${items[selected]}"
        trap - EXIT INT TERM
        stty "$saved" < "$tty"
        return 0
        ;;
      q|Q)
        trap - EXIT INT TERM
        stty "$saved" < "$tty"
        return 1
        ;;
    esac
  done
}

while (($#)); do
  case "$1" in
    --device) (($# >= 2)) || { echo "--device requires an ID" >&2; exit 2; }; DEVICE_ID=$2; shift 2 ;;
    --serial) (($# >= 2)) || { echo "--serial requires a serial" >&2; exit 2; }; ADB_SERIAL=$2; shift 2 ;;
    --root) (($# >= 2)) || { echo "--root requires a path" >&2; exit 2; }; REMOTE_ROOT=$2; shift 2 ;;
    --listen) (($# >= 2)) || { echo "--listen requires IP:PORT" >&2; exit 2; }; LISTEN=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [[ -z "$DEVICE_ID" ]]; then DEVICE_ID=$1
      elif [[ -z "$ADB_SERIAL" ]]; then ADB_SERIAL=$1
      else echo "unexpected argument: $1" >&2; exit 2
      fi
      shift
      ;;
  esac
done

for tool in cargo python3 adb; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
[[ "$REMOTE_ROOT" =~ ^[A-Za-z0-9_./-]+$ ]] || { echo "invalid remote root" >&2; exit 2; }
[[ "$LISTEN" =~ ^[A-Za-z0-9_.:-]+$ ]] || { echo "invalid listen address" >&2; exit 2; }

if [[ -z "$DEVICE_ID" ]]; then
  device_items=()
  while IFS= read -r entry; do
    [[ -n "$entry" ]] && device_items+=("$entry")
  done < <(python3 - "$MANIFEST" <<'PY'
import json, sys
for entry in json.load(open(sys.argv[1], encoding="utf-8"))["devices"]:
    print(f"{entry['device']} — {', '.join(entry.get('models', []))}")
PY
  )
  ((${#device_items[@]} > 0)) || { echo "manifest has no stations" >&2; exit 1; }
  arrow_select "Выберите станцию для сборки:" "${device_items[@]}" || exit 1
  DEVICE_ID=${REPLY%% — *}
fi
[[ "$DEVICE_ID" =~ ^[A-Za-z0-9_.-]+$ ]] || { echo "invalid device ID" >&2; exit 2; }
python3 - "$MANIFEST" "$DEVICE_ID" <<'PY'
import json, sys
manifest, wanted = sys.argv[1:]
data = json.load(open(manifest, encoding="utf-8"))
entries = [x for x in data.get("devices", []) if x.get("device") == wanted]
if len(entries) != 1:
    raise SystemExit(f"unsupported or ambiguous station: {wanted}")
PY

if [[ -z "$ADB_SERIAL" ]]; then
  devices=()
  while IFS= read -r serial; do
    [[ -n "$serial" ]] && devices+=("$serial")
  done < <(adb devices | awk 'NR > 1 && $2 == "device" { print $1 }')
  case "${#devices[@]}" in
    0) echo "no online ADB station found" >&2; exit 1 ;;
    1) ADB_SERIAL=${devices[0]} ;;
    *)
      arrow_select "Выберите ADB-станцию:" "${devices[@]}" || exit 1
      ADB_SERIAL=$REPLY
      ;;
  esac
fi
[[ "$(adb -s "$ADB_SERIAL" get-state 2>/dev/null)" == device ]] || {
  echo "ADB station is not online: $ADB_SERIAL" >&2; exit 1;
}

hostname=$(adb -s "$ADB_SERIAL" shell uname -n | tr -d '\r')
python3 - "$MANIFEST" "$DEVICE_ID" "$hostname" <<'PY'
import json, re, sys
manifest, wanted, hostname = sys.argv[1:]
entry = next(x for x in json.load(open(manifest, encoding="utf-8"))["devices"] if x["device"] == wanted)
model = re.sub(r"[-_][A-Z0-9]{4}$", "", hostname)
model_id = re.sub(r"[^a-z0-9_-]", "-", model.lower())
model_id = re.sub(r"-+", "-", model_id).strip("-")
aliases = entry.get("models", [wanted])
if model_id not in aliases:
    raise SystemExit(f"{wanted} does not support station model {model!r} ({model_id!r})")
print(f"Matched {wanted} to {model!r}")
PY

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/luna-build-deploy.XXXXXX")
trap 'rm -rf -- "$WORK_DIR"' EXIT

echo "Building $DEVICE_ID for $hostname..."
python3 - "$ROOT" "$MANIFEST" "$WORK_DIR/dist" "$DEVICE_ID" <<'PY'
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

root, manifest_path, dist, requested = map(Path, sys.argv[1:])
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
entries = [item for item in manifest["devices"] if item["device"] == str(requested)]
if len(entries) != 1:
    raise SystemExit(f"unsupported or ambiguous station: {requested}")
entry = entries[0]
cargo_manifest = (root / entry["manifest_path"]).resolve()
if not cargo_manifest.is_file() or root not in cargo_manifest.parents:
    raise SystemExit(f"invalid station manifest: {cargo_manifest}")

env = os.environ.copy()
env.setdefault("CARGO_TARGET_DIR", str(root / "target"))
linkers = {
    "aarch64-unknown-linux-musl": "aarch64-linux-musl-gcc",
    "armv7-unknown-linux-musleabihf": "arm-linux-musleabihf-gcc",
}
linker = shutil.which(linkers.get(entry["target"], ""))
if linker:
    env["CARGO_TARGET_" + entry["target"].upper().replace("-", "_") + "_LINKER"] = linker
command = [
    "cargo", "build", "--release", "--locked", "--target", entry["target"],
    "--manifest-path", str(cargo_manifest), "--bin", entry["binary"],
]
print(" ".join(command), flush=True)
subprocess.run(command, cwd=root, env=env, check=True)
source = root / "target" / entry["target"] / "release" / entry["binary"]
if not source.is_file():
    raise SystemExit(f"missing build output: {source}")
assets = Path(dist) / "assets"
assets.mkdir(parents=True, exist_ok=True)
destination = assets / entry["asset"]
shutil.copy2(source, destination)
destination.chmod(destination.stat().st_mode | 0o111)
entry = dict(entry)
entry["filepath"] = f"assets/{destination.name}"
entry["sha256sum"] = hashlib.sha256(destination.read_bytes()).hexdigest()
Path(dist).mkdir(parents=True, exist_ok=True)
(Path(dist) / "manifest.json").write_text(
    json.dumps({"schema_version": 1, "devices": [entry]}, indent=2) + "\n",
    encoding="utf-8",
)
(Path(dist) / "SHA256SUMS").write_text(
    f"{entry['sha256sum']}  {destination.name}\n", encoding="utf-8"
)
PY
asset=$(python3 - "$WORK_DIR/dist/manifest.json" <<'PY'
import json, sys
entries = json.load(open(sys.argv[1], encoding="utf-8"))["devices"]
if len(entries) != 1:
    raise SystemExit("build did not produce exactly one station asset")
print(entries[0]["asset"])
PY
)
binary="$WORK_DIR/dist/assets/$asset"
[[ -f "$binary" ]] || { echo "missing build asset: $binary" >&2; exit 1; }
echo "Built $binary"

remote_script="$WORK_DIR/remote-start.sh"
cat > "$remote_script" <<'REMOTE'
set -eu
root="$LUNA_ROOT"
binary="$root/bin/luna"
staged="$root/bin/luna.new"
backup="$root/bin/luna.previous"
pidfile="$root/luna.pid"
logfile="$root/luna.log"
data="$root/data"

mount -o remount,rw / 2>/dev/null || true
mkdir -p "$root/bin" "$data"
chmod 0755 "$staged"

is_luna_pid() {
  pid=$1
  [ -r "/proc/$pid/cmdline" ] || return 1
  cmd=$(tr '\000' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)
  case "$cmd" in
    */luna\ --listen*|luna\ --listen*|*\ luna\ --listen*|*/luna-*\ --listen*|luna-*\ --listen*|*\ luna-*\ --listen*)
      case "$cmd" in
        */luna-evread\ *|*/luna-iohub2\ *|*\ luna-evread\ *|*\ luna-iohub2\ *) return 1 ;;
        *) return 0 ;;
      esac
      ;;
    *) return 1 ;;
  esac
}

stop_luna_pid() {
  pid=$1
  case "$pid" in ''|*[!0-9]*) return 0 ;; esac
  kill -0 "$pid" 2>/dev/null || return 0
  if ! is_luna_pid "$pid"; then
    echo "Skipping PID $pid: it is not the Luna main process" >&2
    return 0
  fi
  case "$pid" in ''|*[!0-9]*) echo "invalid Luna pidfile" >&2; exit 1 ;; esac
  echo "Stopping Luna PID $pid"
  kill "$pid" 2>/dev/null || true
  attempt=0
  while kill -0 "$pid" 2>/dev/null && [ "$attempt" -lt 50 ]; do
    sleep 0.1
    attempt=$((attempt + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "Luna PID $pid did not stop; forcing termination" >&2
    kill -9 "$pid" 2>/dev/null || true
  fi
}

if [ -f "$pidfile" ]; then
  pid=$(cat "$pidfile")
  stop_luna_pid "$pid"
  rm -f "$pidfile"
fi

# YAM may have started Luna from its own package directory, while a previous
# manual deployment may have used another directory.  The common lock in the
# binary handles new versions; this scan also cleans up older Luna binaries
# that predate the lock.
for proc in /proc/[0-9]*; do
  pid=${proc##*/}
  [ "$pid" = "$$" ] && continue
  if is_luna_pid "$pid"; then
    stop_luna_pid "$pid"
  fi
done

[ -f "$binary" ] && cp "$binary" "$backup"
mv "$staged" "$binary"
nohup "$binary" --listen "$LUNA_LISTEN" --data-dir "$data" >"$logfile" 2>&1 </dev/null &
echo $! > "$pidfile"
sleep 1
pid=$(cat "$pidfile")
kill -0 "$pid" 2>/dev/null || { cat "$logfile" >&2; exit 1; }
REMOTE

adb -s "$ADB_SERIAL" shell "mount -o remount,rw / 2>/dev/null || true; mkdir -p '$REMOTE_ROOT/bin' '$REMOTE_ROOT/data'"
adb -s "$ADB_SERIAL" push "$binary" "$REMOTE_ROOT/bin/luna.new" >/dev/null
adb -s "$ADB_SERIAL" shell "LUNA_ROOT='$REMOTE_ROOT' LUNA_LISTEN='$LISTEN' sh -s" < "$remote_script"

port="${LISTEN##*:}"
adb forward "tcp:$port" "tcp:$port" >/dev/null
echo "Luna started on $ADB_SERIAL; checking API through localhost:$port..."
curl -fsS "http://127.0.0.1:$port/api/device"
printf '\nDeployed successfully.\n'
