[CmdletBinding()]
param(
    [string]$Version = $(if ($env:LUNA_VERSION) { $env:LUNA_VERSION } else { "latest" }),
    [string]$Serial = $(if ($env:ADB_SERIAL) { $env:ADB_SERIAL } else { "" }),
    [string]$Platform = $(if ($env:LUNA_PLATFORM) { $env:LUNA_PLATFORM } else { "" }),
    [string]$Repository = $(if ($env:LUNA_REPOSITORY) { $env:LUNA_REPOSITORY } else { "https://github.com/nyakokitsu/luna" }),
    [string]$RemoteRoot = $(if ($env:LUNA_REMOTE_ROOT) { $env:LUNA_REMOTE_ROOT } else { "/data/quazar/luna" }),
    [switch]$Uninstall
)
$ErrorActionPreference = "Stop"
function Log([string]$Message) { Write-Host ("[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $Message) }
function Require-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) { throw "$Name is required. Install Android Platform Tools and add adb to PATH." }
}
function Invoke-AdbShell([string]$Command) {
    Log "ADB shell: $Command"
    $result = & adb -s $Serial shell $Command 2>&1
    if ($LASTEXITCODE -ne 0) { throw "adb shell failed: $result" }
    Log "ADB shell complete"
    return ($result -join [Environment]::NewLine)
}
function Invoke-AdbOptional([string]$Command) {
    try { return Invoke-AdbShell $Command } catch { Log "optional probe failed: $Command"; return "" }
}
function Compact([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) { return "" }
    $value = $Value.Trim() -creplace '[A-Z]{4}$', ''
    # Отбрасываем серийный суффикс вроде "_F6D0" или "-16AK", только если
    # в нём есть цифра. "-mini", "-lite", "-midi" и "-gen2" — части имени модели.
    $value = $value.ToLowerInvariant() -creplace '[-_](?!gen[0-9]$)(?=[a-z]*[0-9])[a-z0-9]{4}$', ''
    return ($value -creplace '[^a-z0-9]', '')
}
function Get-DetectedDtPlatform {
    $dtRaw = Invoke-AdbOptional 'cat /proc/device-tree/compatible /proc/device-tree/amlogic-dt-id 2>/dev/null | tr "\000" "\n"'
    $compact = ("$dtRaw".ToLowerInvariant() -replace '[\r\n \t]+', '')
    if ($compact -match 'yandex,station-mini2|station-mini2|axg_mini2') { return "yandexmini_2" }
    elseif ($compact -match 'yandex,cucumber|a5_cucumber|cucumber') { return "cucumber" }
    elseif ($compact -match 'yandex,station-midi') { return "yandexstation_2" }
    elseif ($compact -match 'yandex,orion|a5_orion|orion') { return "orion" }
    elseif ($compact -match 'yandex,pickle|pickle|amlogic,a4') { return "pickle" }
    elseif ($compact -match 'yandex,bergamot|yandex,station-mini3|bergamot') { return "bergamot" }
    elseif ($compact -match 'yandex,saturn|saturn') { return "saturn" }
    elseif ($compact -match 'yandex,mango|mango|amlogic,a1') { return "mango" }
    elseif ($compact -match 'yandex,plum|yandex,prune|plum|prune|station-lite2') { return "plum" }
    elseif ($compact -match 'yandex,yandexmicro|yandex.*station.*micro|yandexmicro|blackthorn') { return "yandexmicro" }
    elseif ($compact -match 'axg_s420|amlogic,axg') { return "yandexmini" }
    return ""
}
function Get-Aliases($Entry) {
    return @(@($Entry.models) + @($Entry.device, $Entry.codename, $Entry.yam_target) |
        Where-Object { $_ } | ForEach-Object { ([string]$_).Trim().ToLowerInvariant() -creplace '[^a-z0-9]', '' })
}
Require-Command "adb"
if ($RemoteRoot -notmatch '^[A-Za-z0-9_./-]+$') { throw "Invalid remote root" }
$devices = @((& adb devices) | Where-Object { $_ -match '^([^\s]+)\s+device$' } | ForEach-Object { $Matches[1] })
if (-not $Serial) {
    if ($devices.Count -eq 0) { throw "No ADB station found." }
    if ($devices.Count -eq 1) { $Serial = $devices[0] }
    else {
        for ($i = 0; $i -lt $devices.Count; $i++) { Write-Host ("{0}) {1}" -f ($i + 1), $devices[$i]) }
        $choice = [int](Read-Host "Station number")
        if ($choice -lt 1 -or $choice -gt $devices.Count) { throw "Invalid station number" }
        $Serial = $devices[$choice - 1]
    }
}
if ((& adb -s $Serial get-state 2>$null) -ne "device") { throw "ADB station is offline: $Serial" }
Log "Connected to station $Serial"
$url = if ($Version -eq "latest") { "$Repository/releases/latest/download" } else {
    if ($Version -notmatch '^v') { $Version = "v$Version" }
    "$Repository/releases/download/$Version"
}
$temp = Join-Path ([IO.Path]::GetTempPath()) ("luna-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    if ($Uninstall) {
        $remoteText = @'
set -eu
root="$LUNA_ROOT"; init=/etc/init.d/S99luna
[ -x "$init" ] && "$init" stop || true
rm -f "$init" "$root/bin/luna" "$root/bin/luna.previous" "$root/luna.pid"
echo "Luna removed"
'@
        $remoteText = ($remoteText -replace "`r`n", "`n") + "`n"
        [IO.File]::WriteAllText("$temp/remote.sh", $remoteText, [Text.Encoding]::ASCII)
        Get-Content "$temp/remote.sh" -Raw | & adb -s $Serial shell "LUNA_ROOT='$RemoteRoot' sh -s"
        if ($LASTEXITCODE -ne 0) { throw "Remote uninstall failed" }
        Write-Host "Luna removed from $Serial"
        return
    }
    Log "Downloading release metadata"
    Invoke-WebRequest "$url/manifest.json" -OutFile "$temp/manifest.json" -UseBasicParsing
    Invoke-WebRequest "$url/SHA256SUMS" -OutFile "$temp/SHA256SUMS" -UseBasicParsing
    $manifest = Get-Content "$temp/manifest.json" -Raw | ConvertFrom-Json
    
    if (-not [string]::IsNullOrWhiteSpace($Platform)) {
        $targetPlatform = $Platform.Trim()
        Log "Using explicitly provided platform: $targetPlatform"
    } else {
        Log "Detecting station"
        $targetPlatform = Get-DetectedDtPlatform
        if ($targetPlatform) {
            Log "Auto-detected platform from device-tree: $targetPlatform"
        } else {
            $targetPlatform = (Invoke-AdbShell 'uname -n').Trim()
            Log "Auto-detected hostname: $targetPlatform"
        }
    }

    # Те же правила, что и в install.sh: точный alias имеет приоритет,
    # подстрока используется только как запасной вариант автоопределения.
    $target = Compact $targetPlatform
    $found = @(@($manifest.devices) | Where-Object { (Get-Aliases $_) -contains $target })
    if ($found.Count -eq 0 -and [string]::IsNullOrWhiteSpace($Platform)) {
        $found = @(@($manifest.devices) | Where-Object {
            $aliases = Get-Aliases $_
            @($aliases | Where-Object { $_.Contains($target) -or $target.Contains($_) }).Count -gt 0
        })
    }
    if ($found.Count -ne 1) { throw "Unsupported or ambiguous station. Target: $targetPlatform" }
    $entry = $found[0]
    $asset = $entry.asset
    Log "Selected $($entry.device) ($($entry.codename)) [Asset: $asset]"
    Log "Downloading $asset"
    Invoke-WebRequest "$url/$asset" -OutFile "$temp/$asset" -UseBasicParsing
    $sumLine = Get-Content "$temp/SHA256SUMS" | Where-Object { $_ -match "\s$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $sumLine) { throw "No checksum found for $asset" }
    $expected = ($sumLine -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash "$temp/$asset" -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { throw "Checksum mismatch for $asset" }
    Log "Checksum verified"
    $init = @'
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
stop) [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true; rm -f "$pidfile" ;;
restart) "$0" stop; "$0" start ;;
*) exit 2 ;;
esac
'@
    $remoteText = @'
set -eu
root="$LUNA_ROOT"; binary="$root/bin/luna"; init=/etc/init.d/S99luna
mount -o remount,rw / 2>/dev/null || true
mount | grep ' on / ' | grep -q '(rw' || { echo "root filesystem is not writable" >&2; exit 1; }
mkdir -p "$root/bin" "$root/data"; chmod 0755 "$root/bin/luna.new"
[ ! -f "$init" ] || grep -q '^# Managed by Luna$' "$init" || { echo "refusing unmanaged $init" >&2; exit 1; }
[ -x "$init" ] && "$init" stop || true
[ -f "$binary" ] && cp "$binary" "$root/bin/luna.previous"
mv "$root/bin/luna.new" "$binary"
cat >"$init" <<'INIT'
__INIT__
INIT
chmod 0755 "$init"; "$init" start
echo "Luna installed"
'@
    $remoteText = ($remoteText.Replace("__INIT__", $init) -replace "`r`n", "`n") + "`n"
    Log "Uploading $asset to $RemoteRoot"
    [IO.File]::WriteAllText("$temp/remote.sh", $remoteText, [Text.Encoding]::ASCII)
    & adb -s $Serial push "$temp/$asset" "$RemoteRoot/bin/luna.new" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "adb push failed" }
    Log "Starting Luna on the station"
    Get-Content "$temp/remote.sh" -Raw | & adb -s $Serial shell "LUNA_ROOT='$RemoteRoot' sh -s"
    if ($LASTEXITCODE -ne 0) { throw "Remote installation failed" }
    Log "Luna installed on $Serial ($($entry.device)); API port 8099"
} finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
