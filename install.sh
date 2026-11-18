#!/bin/sh
set -eu

VERSION=${LUNA_VERSION:-latest}
PLATFORM=${LUNA_PLATFORM:-}
BOOT_HOOK=1
LISTEN=${LUNA_LISTEN:-127.0.0.1:8100}
UNINSTALL=0
QUIET=0
case "${LUNA_TELEMETRY:-ask}" in
  on|yes|1|true) TELEMETRY=on ;;
  off|no|0|false) TELEMETRY=off ;;
  *) TELEMETRY=ask ;;
esac

usage() {
  cat <<'USAGE'
Luna installer — runtime для станций Яндекса.

Использование:
  install.sh [VERSION] [параметры]

Аргументы:
  VERSION                тег релиза (по умолчанию latest)

Параметры:
  -p, --platform NAME    платформа станции, если автодетект не нужен
      --listen ADDR      адрес API (по умолчанию 127.0.0.1:8100)
      --no-boot-hook     ставить только в /data, без /etc/init.d
      --uninstall        удалить Luna со станции
      --telemetry        включить анонимную статистику без вопроса
      --no-telemetry     не отправлять анонимную статистику
  -q, --quiet            показывать только предупреждения и ошибки
  -h, --help             эта справка

Переменные окружения:
  ADB_SERIAL             серийный номер станции, если подключено несколько
  LUNA_VERSION           тег релиза
  LUNA_PLATFORM          платформа станции
  LUNA_LISTEN            адрес API
  LUNA_TELEMETRY         on, off или ask (по умолчанию ask)
  LUNA_REMOTE_ROOT       каталог установки (по умолчанию /data/quazar/luna)
  LUNA_REPOSITORY        репозиторий релизов
  NO_COLOR               отключить цвета

Примеры:
  sh install.sh
  ADB_SERIAL=xyz sh install.sh --platform yandexmini3
  sh install.sh 0.2.8 --listen 127.0.0.1:8099
  sh install.sh --uninstall
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --platform|-p)
      [ "$#" -ge 2 ] || { echo "option $1 requires a value" >&2; exit 2; }
      PLATFORM=$2; shift 2 ;;
    --no-boot-hook) BOOT_HOOK=0; shift ;;
    --listen)
      [ "$#" -ge 2 ] || { echo "option $1 requires a value" >&2; exit 2; }
      LISTEN=$2; shift 2 ;;
    --version)
      [ "$#" -ge 2 ] || { echo "option $1 requires a value" >&2; exit 2; }
      VERSION=$2; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --telemetry) TELEMETRY=on; shift ;;
    --no-telemetry) TELEMETRY=off; shift ;;
    --quiet|-q) QUIET=1; shift ;;
    --help|-h) usage; exit 0 ;;
    -*)
      echo "unknown option: $1" >&2
      echo "run $0 --help for usage" >&2
      exit 2 ;;
    *)
      if [ "$VERSION" = "latest" ] && [ -z "${LUNA_VERSION:-}" ]; then
        VERSION=$1
      elif [ -z "$PLATFORM" ]; then
        PLATFORM=$1
      else
        echo "unexpected argument: $1" >&2
        exit 2
      fi
      shift ;;
  esac
done

REPOSITORY=${LUNA_REPOSITORY:-https://github.com/nyakokitsu/luna}
ROOT=${LUNA_REMOTE_ROOT:-/data/quazar/luna}
SERIAL=${ADB_SERIAL:-}
case "$LISTEN" in ''|*[!A-Za-z0-9_.:-]*) echo "invalid listen address: $LISTEN" >&2; exit 2 ;; esac
case "$ROOT" in ''|*[!A-Za-z0-9_./-]*) echo "invalid remote root: $ROOT" >&2; exit 2 ;; esac

# --- оформление вывода -------------------------------------------------------
if [ "$QUIET" -eq 0 ] && [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
  STYLE=1
else
  STYLE=0
fi
if [ "$STYLE" -eq 1 ]; then
  BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RED=$(printf '\033[31m')
  GREEN=$(printf '\033[32m'); YELLOW=$(printf '\033[33m'); BLUE=$(printf '\033[34m'); NC=$(printf '\033[0m')
else
  BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; BLUE=''; NC=''
fi
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
  *UTF-8*|*utf8*|*UTF8*) UNICODE=1 ;;
  *) UNICODE=0 ;;
esac
BULLET='-'
BAR_FULL='#'
BAR_EMPTY='.'
if [ "$UNICODE" -eq 1 ]; then
  BULLET=$(printf '\342\200\272')
  BAR_FULL=$(printf '\342\226\210')
  BAR_EMPTY=$(printf '\342\226\221')
fi

STEP_TOTAL=7
STEP_INDEX=0
STEP_START=0
TOTAL_START=$(date +%s)
PROGRESS_OPEN=0

say() { [ "$QUIET" -eq 1 ] || printf '%s\n' "$*" >&2; }
info() { [ "$QUIET" -eq 1 ] || printf '   %s%s%s %s\n' "$DIM" "$BULLET" "$NC" "$*" >&2; }
warn() { printf '   %s!%s %s\n' "$YELLOW" "$NC" "$*" >&2; }
die() {
  code=${2:-1}
  [ "$PROGRESS_OPEN" -eq 1 ] && printf '\n' >&2
  printf '%serror:%s %s\n' "$RED" "$NC" "$1" >&2
  exit "$code"
}
stage() {
  STEP_INDEX=$((STEP_INDEX + 1))
  STEP_START=$(date +%s)
  [ "$QUIET" -eq 1 ] && return 0
  printf '\n%s[%d/%d]%s %s%s%s\n' "$DIM" "$STEP_INDEX" "$STEP_TOTAL" "$NC" "$BOLD" "$1" "$NC" >&2
}
stage_done() {
  [ "$QUIET" -eq 1 ] && return 0
  took=$(( $(date +%s) - STEP_START ))
  printf '   %sok%s %s%s%s\n' "$GREEN" "$NC" "$DIM" "(${took}s)" "$NC" >&2
}
human_size() {
  awk -v n="${1:-0}" 'BEGIN{u[1]="B";u[2]="KB";u[3]="MB";u[4]="GB";i=1;while(n>=1024&&i<4){n/=1024;i++};if(i==1)printf "%d %s",n,u[i];else printf "%.1f %s",n,u[i]}'
}
progress_line() {
  current=$1; total=$2; label=$3
  width=26
  if [ "$total" -gt 0 ] 2>/dev/null; then
    percent=$((current * 100 / total))
  else
    percent=0
  fi
  [ "$percent" -gt 100 ] && percent=100
  filled=$((percent * width / 100))
  bar=''
  i=0
  while [ "$i" -lt "$width" ]; do
    if [ "$i" -lt "$filled" ]; then bar="$bar$BAR_FULL"; else bar="$bar$BAR_EMPTY"; fi
    i=$((i + 1))
  done
  PROGRESS_OPEN=1
  if [ "$total" -gt 0 ] 2>/dev/null; then
    printf '\r   %s %s %3d%% %s / %s%s' "$label" "$bar" "$percent" "$(human_size "$current")" "$(human_size "$total")" "$NC" >&2
  else
    printf '\r   %s %s %s%s' "$label" "$bar" "$(human_size "$current")" "$NC" >&2
  fi
}

TMP=''
cleanup() {
  status=$?
  [ "$PROGRESS_OPEN" -eq 1 ] && printf '\n' >&2
  [ -z "$TMP" ] || rm -rf "$TMP"
  if [ "$status" -ne 0 ] && [ "$status" -ne 130 ]; then
    printf '%sinstallation failed%s %s(exit %d)%s\n' "$RED" "$NC" "$DIM" "$status" "$NC" >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case "$VERSION" in
  latest) URL="$REPOSITORY/releases/latest/download" ;;
  v*) URL="$REPOSITORY/releases/download/$VERSION" ;;
  *) URL="$REPOSITORY/releases/download/v$VERSION" ;;
esac

[ "$QUIET" -eq 1 ] || {
  printf '\n%sLuna installer%s %s%s%s\n' "$BOLD" "$NC" "$DIM" "version $VERSION" "$NC" >&2
  printf '   %s%s%s\n' "$DIM" "$URL" "$NC" >&2
}

STEP_TOTAL=7
[ "$UNINSTALL" -eq 1 ] && STEP_TOTAL=3

stage "Проверяем окружение"
command -v adb >/dev/null || die "adb не найден. Установите Android Platform Tools." 4
if [ "$UNINSTALL" -eq 0 ]; then
  command -v python3 >/dev/null || die "python3 не найден" 4
  if command -v curl >/dev/null 2>&1; then DOWNLOAD=curl
  elif command -v wget >/dev/null 2>&1; then DOWNLOAD=wget
  else die "нужен curl или wget" 4; fi
  info "инструменты: adb, python3, $DOWNLOAD"
fi
TMP=$(mktemp -d "${TMPDIR:-/tmp}/luna-install.XXXXXX")

download() {
  url=$1; dest=$2; label=${3:-file}
  total=0
  if [ "$STYLE" -eq 1 ]; then
    if [ "$DOWNLOAD" = curl ]; then
      total=$(curl -fsIL --proto '=https,http' --tlsv1.2 "$url" 2>/dev/null |
        awk 'tolower($1) == "content-length:" { gsub("\r", "", $2); size = $2 } END { print size + 0 }')
    else
      total=$(wget --spider -S "$url" 2>&1 |
        awk 'tolower($1) == "content-length:" { gsub("\r", "", $2); size = $2 } END { print size + 0 }')
    fi
    case "$total" in ''|*[!0-9]*) total=0 ;; esac
    if [ "$DOWNLOAD" = curl ]; then
      curl -fsSL --proto '=https,http' --tlsv1.2 "$url" -o "$dest" &
    else
      wget -q -O "$dest" "$url" &
    fi
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
      current=$(wc -c <"$dest" 2>/dev/null || printf 0)
      progress_line "$current" "$total" "$label"
      sleep 0.2 2>/dev/null || sleep 1
    done
    wait "$pid" || { printf '\n' >&2; PROGRESS_OPEN=0; die "не удалось скачать $url" 8; }
    current=$(wc -c <"$dest" 2>/dev/null || printf 0)
    progress_line "$current" "$total" "$label"
    printf '\n' >&2
    PROGRESS_OPEN=0
  else
    if [ "$DOWNLOAD" = curl ]; then
      curl -fsSL --proto '=https,http' --tlsv1.2 "$url" -o "$dest" || die "не удалось скачать $url" 8
    else
      wget -q -O "$dest" "$url" || die "не удалось скачать $url" 8
    fi
    info "$label: $(human_size "$(wc -c <"$dest")")"
  fi
}
stage_done

stage "Подключаемся к станции"
if [ -z "$SERIAL" ]; then
  devices=$(adb devices </dev/null | awk 'NR > 1 && $2 == "device" {print $1}')
  count=$(printf '%s\n' "$devices" | sed '/^\s*$/d' | wc -l | tr -d ' ')
  [ "$count" -gt 0 ] || die "не найдено ни одной станции по ADB" 6
  if [ "$count" -eq 1 ]; then
    SERIAL=$(printf '%s\n' "$devices" | head -n 1)
  else
    [ "$QUIET" -eq 1 ] || printf '%s\n' "$devices" | awk '{printf "   %d) %s\n", NR, $1}' >&2
    printf '   выберите станцию: ' >&2
    read -r n </dev/tty
    SERIAL=$(printf '%s\n' "$devices" | sed -n "${n}p")
    [ -n "$SERIAL" ] || die "станция с номером $n не найдена" 6
  fi
fi
adb -s "$SERIAL" get-state </dev/null 2>/dev/null | grep -qx device || die "станция $SERIAL не отвечает" 6
info "станция: $SERIAL"
stage_done

if [ "$UNINSTALL" -eq 1 ]; then
  stage "Удаляем Luna"
  REMOTE="$TMP/remote.sh"
  cat >"$REMOTE" <<'REMOTE'
set -eu
root="$LUNA_ROOT"; init=/etc/init.d/S99luna
if [ -x "$init" ]; then "$init" stop || true; fi
rm -f "$init" "$root/bin/luna" "$root/bin/luna.previous" "$root/luna.pid"
echo "Luna removed"
REMOTE
  adb -s "$SERIAL" shell "LUNA_ROOT='$ROOT' sh -s" <"$REMOTE" || die "удалённая остановка не удалась" 9
  stage_done
  say ""
  say "Luna удалена со станции $SERIAL"
  exit 0
fi

stage "Скачиваем метаданные релиза"
download "$URL/SHA256SUMS" "$TMP/SHA256SUMS" "SHA256SUMS"
download "$URL/manifest.json" "$TMP/manifest.json" "manifest.json"
stage_done

stage "Определяем станцию"
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
  TARGET="$PLATFORM"
  EXPLICIT=1
  info "платформа задана явно: $PLATFORM"
else
  TARGET=$(detect_dt_platform || true)
  if [ -n "$TARGET" ]; then
    info "платформа из device-tree: $TARGET"
  else
    TARGET=$(adb_optional 'uname -n' | tr -d '\r')
    info "имя хоста станции: $TARGET"
  fi
  EXPLICIT=0
fi

[ -n "$TARGET" ] || die "не удалось определить платформу или имя хоста" 7

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
    x = re.sub(r"[-_](?!gen[0-9]$|[0-9]{4}$)(?=[a-z]*[0-9])[a-z0-9]{4}$", "", x.lower())
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
) || die "станция не поддерживается или определяется неоднозначно: $TARGET" 7

IFS="$(printf '\t')" read -r DEVICE ASSET CODENAME <<EOF
$RECORD
EOF
info "модель: $DEVICE${CODENAME:+ ($CODENAME)}"
stage_done

stage "Скачиваем $ASSET"
download "$URL/$ASSET" "$TMP/$ASSET" "$ASSET"
stage_done

stage "Проверяем контрольную сумму"
expected=$(awk -v name="$(basename "$TMP/$ASSET")" '$2 == name {print $1}' "$TMP/SHA256SUMS")
[ -n "$expected" ] || die "в SHA256SUMS нет записи для $ASSET" 5
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$TMP/$ASSET" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
else actual=$(openssl dgst -sha256 "$TMP/$ASSET" | awk '{print $NF}'); fi
[ "$actual" = "$expected" ] || die "контрольная сумма не совпала для $ASSET" 5
info "sha256: $expected"
stage_done

stage "Устанавливаем на станцию"
REMOTE="$TMP/remote.sh"
cat >"$REMOTE" <<'REMOTE'
set -eu
root="$LUNA_ROOT"; binary="$root/bin/luna"; staged="$root/bin/luna.new"; pidfile="$root/luna.pid"; logfile="$root/luna.log"; init=/etc/init.d/S99luna

# Explicit /data-only installation for read-only retail firmware. The owner
# supplies persistence through an existing, trusted boot mechanism.
if [ "${LUNA_BOOT_HOOK:-1}" = 0 ]; then
  mkdir -p "$root/bin" "$root/data"
  chmod 0755 "$staged"
  [ ! -f "$binary" ] || cp "$binary" "$root/bin/luna.previous"
  mv "$staged" "$binary"
  nohup "$binary" --listen "$LUNA_LISTEN" --data-dir "$root/data" >"$root/luna.log" 2>&1 </dev/null &
  echo $! >"$pidfile"
  sleep 1
  kill -0 "$(cat "$pidfile")"
  echo "Luna started without a system boot hook; configure owner-managed persistence."
  exit 0
fi
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
root=__LUNA_ROOT__; binary=$root/bin/luna; pidfile=$root/luna.pid; logfile=$root/luna.log
case "${1:-start}" in
start)
  [ -x "$binary" ] || exit 0; mkdir -p "$root/data"
  if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then exit 0; fi
  rm -f "$pidfile"
  if command -v start-stop-daemon >/dev/null 2>&1; then
    start-stop-daemon --start --background --make-pidfile --pidfile "$pidfile" --startas "$binary" -- --listen __LUNA_LISTEN__ --data-dir "$root/data"
  else
    nohup "$binary" --listen __LUNA_LISTEN__ --data-dir "$root/data" >"$logfile" 2>&1 </dev/null & echo $! >"$pidfile"
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
sed "s|__LUNA_ROOT__|$ROOT|g; s|__LUNA_LISTEN__|$LISTEN|g" "$REMOTE" >"$REMOTE.configured"
mv "$REMOTE.configured" "$REMOTE"
adb -s "$SERIAL" shell "mkdir -p '$ROOT/bin'" </dev/null >/dev/null || die "не удалось создать $ROOT/bin" 9
info "загрузка $(human_size "$(wc -c <"$TMP/$ASSET")") в $ROOT/bin"
adb -s "$SERIAL" push "$TMP/$ASSET" "$ROOT/bin/luna.new" </dev/null >/dev/null || die "adb push не удался" 9
adb -s "$SERIAL" shell "LUNA_ROOT='$ROOT' LUNA_LISTEN='$LISTEN' LUNA_BOOT_HOOK='$BOOT_HOOK' sh -s" <"$REMOTE" ||
  die "удалённая установка завершилась ошибкой" 9
stage_done

# Анонимная статистика: только явное согласие. Решение записывает сама Luna
# (`telemetry on|off`), чтобы веб-панель не спрашивала второй раз. Без ответа
# файл не создаётся и панель спросит при первом открытии.
telemetry_decide() {
  case "$TELEMETRY" in
    on) return 0 ;;
    off) return 1 ;;
  esac
  TELEMETRY=''
  [ "$QUIET" -eq 0 ] || return 1
  [ -r /dev/tty ] || return 1
  printf '\n   Отправлять анонимную статистику Luna?\n' >&2
  printf '   %sмодель, версия, модули, настройки, ошибки и нагрузка станции;%s\n' "$DIM" "$NC" >&2
  printf '   %sбез IP, серийного номера, имени хоста и текста комнаты.%s\n' "$DIM" "$NC" >&2
  printf '   Отправлять на stats.x-servers.ru? [y/N]: ' >&2
  answer=''
  read -r answer </dev/tty || return 1
  case "$answer" in
    y|Y|yes|YES|Yes|д|Д|да|ДА|Да) TELEMETRY=on; return 0 ;;
    *) TELEMETRY=off; return 1 ;;
  esac
}

if telemetry_decide; then
  info "включаю анонимную статистику"
  if telemetry_out=$(adb -s "$SERIAL" shell "$ROOT/bin/luna --data-dir '$ROOT/data' --model '$DEVICE' telemetry on" </dev/null 2>&1); then
    [ "$QUIET" -eq 1 ] || printf '   %s\n' "$telemetry_out" | sed "s/^/   $DIM/" >&2
  else
    warn "станция не подтвердила включение статистики"
  fi
elif [ "$TELEMETRY" = off ]; then
  adb -s "$SERIAL" shell "$ROOT/bin/luna --data-dir '$ROOT/data' telemetry off" </dev/null >/dev/null 2>&1 ||
    warn "не удалось записать отказ от статистики"
  [ "$QUIET" -eq 1 ] || info "анонимная статистика выключена"
fi

total=$(( $(date +%s) - TOTAL_START ))
if [ "$QUIET" -eq 0 ]; then
  printf '\n%sLuna установлена%s %s(%ss)%s\n' "$BOLD" "$NC" "$DIM" "$total" "$NC" >&2
  printf '   станция: %s%s%s\n' "$BOLD" "$DEVICE${CODENAME:+ ($CODENAME)}" "$NC" >&2
  printf '   версия:  %s\n' "$VERSION" >&2
  printf '   API:     http://%s/api\n' "$LISTEN" >&2
  printf '   данные:  %s/data\n' "$ROOT" >&2
  printf '   лог:     %s/luna.log\n' "$ROOT" >&2
  case "$TELEMETRY" in
    on) printf '   статистика: %sвключена%s\n\n' "$GREEN" "$NC" >&2 ;;
    off) printf '   статистика: выключена\n\n' >&2 ;;
    *) printf '   статистика: спрошу в веб-панели\n\n' >&2 ;;
  esac
fi
