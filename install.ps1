[CmdletBinding()]
param(
    [string]$Version = $(if ($env:LUNA_VERSION) { $env:LUNA_VERSION } else { "latest" }),
    [string]$Serial = $(if ($env:ADB_SERIAL) { $env:ADB_SERIAL } else { "" }),
    [string]$Platform = $(if ($env:LUNA_PLATFORM) { $env:LUNA_PLATFORM } else { "" }),
    [string]$Repository = $(if ($env:LUNA_REPOSITORY) { $env:LUNA_REPOSITORY } else { "https://github.com/nyakokitsu/luna" }),
    [string]$RemoteRoot = $(if ($env:LUNA_REMOTE_ROOT) { $env:LUNA_REMOTE_ROOT } else { "/data/quazar/luna" }),
    [string]$Listen = $(if ($env:LUNA_LISTEN) { $env:LUNA_LISTEN } else { "127.0.0.1:8100" }),
    [switch]$NoBootHook,
    [switch]$Uninstall,
    [switch]$Quiet
)
$ErrorActionPreference = "Stop"
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

$script:StepIndex = 0
$script:StepTotal = if ($Uninstall) { 3 } else { 7 }
$script:StepStarted = Get-Date
$script:InstallStarted = Get-Date

function Format-Size([long]$Bytes) {
    $units = @("B", "KB", "MB", "GB")
    $value = [double]$Bytes
    $index = 0
    while ($value -ge 1024 -and $index -lt 3) { $value /= 1024; $index++ }
    if ($index -eq 0) { return ("{0} {1}" -f [int]$value, $units[$index]) }
    return ("{0:F1} {1}" -f $value, $units[$index])
}

function Format-Duration([TimeSpan]$Span) {
    if ($Span.TotalSeconds -lt 60) { return ("{0:N0}s" -f $Span.TotalSeconds) }
    return ("{0:N0}m {1:N0}s" -f [math]::Floor($Span.TotalMinutes), $Span.Seconds)
}

function Write-Stage([string]$Title) {
    $script:StepIndex++
    $script:StepStarted = Get-Date
    if ($Quiet) { return }
    Write-Host ""
    Write-Host ("[{0}/{1}] " -f $script:StepIndex, $script:StepTotal) -ForegroundColor DarkGray -NoNewline
    Write-Host $Title -ForegroundColor White
    $percent = [int](($script:StepIndex - 1) * 100 / $script:StepTotal)
    Write-Progress -Id 1 -Activity "Luna installer" -Status $Title -PercentComplete $percent
}

function Write-StageDone {
    if ($Quiet) { return }
    $took = (Get-Date) - $script:StepStarted
    Write-Host "   ok " -ForegroundColor DarkGray -NoNewline
    Write-Host ("({0})" -f (Format-Duration $took)) -ForegroundColor DarkGray
}

function Write-Detail([string]$Message) {
    if ($Quiet) { return }
    Write-Host "   - " -ForegroundColor DarkGray -NoNewline
    Write-Host $Message -ForegroundColor DarkGray
}

function Write-Warn([string]$Message) { Write-Host "   ! $Message" -ForegroundColor Yellow }
function Fail([string]$Message) { Write-Progress -Id 1 -Activity "Luna installer" -Completed; throw $Message }

function Show-DownloadProgress([string]$Label, [long]$Done, [long]$Total) {
    if ($Quiet) { return }
    if ($Total -gt 0) {
        $percent = [math]::Min(100, [int]($Done * 100 / $Total))
        Write-Progress -Id 2 -ParentId 1 -Activity $Label -Status ("{0} / {1}" -f (Format-Size $Done), (Format-Size $Total)) -PercentComplete $percent
    } else {
        Write-Progress -Id 2 -ParentId 1 -Activity $Label -Status (Format-Size $Done)
    }
}

function Get-RemoteFile([string]$Url, [string]$Path, [string]$Label) {
    $client = $null; $stream = $null; $file = $null
    try {
        $client = New-Object System.Net.Http.HttpClient
        $client.Timeout = [TimeSpan]::FromMinutes(10)
        $response = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if (-not $response.IsSuccessStatusCode) {
            Fail ("download failed ({0} {1}): {2}" -f [int]$response.StatusCode, $response.ReasonPhrase, $Url)
        }
        $total = 0L
        if ($null -ne $response.Content.Headers.ContentLength) { $total = [long]$response.Content.Headers.ContentLength }
        $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $file = [IO.File]::Create($Path)
        $buffer = New-Object byte[] 81920
        $done = 0L
        $last = [DateTime]::MinValue
        while (($read = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $file.Write($buffer, 0, $read)
            $done += $read
            if ((([DateTime]::UtcNow - $last).TotalMilliseconds) -ge 120) {
                Show-DownloadProgress $Label $done $total
                $last = [DateTime]::UtcNow
            }
        }
        Show-DownloadProgress $Label $done $total
        Write-Progress -Id 2 -ParentId 1 -Activity $Label -Completed
        if (-not $Quiet) { Write-Detail ("{0}: {1}" -f $Label, (Format-Size $done)) }
    } finally {
        if ($file) { $file.Dispose() }
        if ($stream) { $stream.Dispose() }
        if ($client) { $client.Dispose() }
    }
}

function Require-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Fail "$Name is required. Install Android Platform Tools and add adb to PATH."
    }
}

function Invoke-AdbShell([string]$Command) {
    Write-Verbose "adb shell: $Command"
    $result = & adb -s $Serial shell $Command 2>&1
    if ($LASTEXITCODE -ne 0) { Fail "adb shell failed: $result" }
    return ($result -join [Environment]::NewLine)
}

function Invoke-AdbOptional([string]$Command) {
    try { return Invoke-AdbShell $Command } catch { Write-Verbose "optional probe failed: $Command"; return "" }
}

function Compact([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) { return "" }
    $value = $Value.Trim() -creplace '[A-Z]{4}$', ''
    # Отбрасываем серийный суффикс вроде "_F6D0" или "-16AK", только если
    # в нём есть цифра. "-mini", "-lite", "-midi" и "-gen2" — части имени модели.
    $value = $value.ToLowerInvariant() -creplace '[-_](?!gen[0-9]$|[0-9]{4}$)(?=[a-z]*[0-9])[a-z0-9]{4}$', ''
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
if ($Listen -notmatch "^[A-Za-z0-9_.:-]+$") { Fail "Invalid listen address: $Listen" }
if ($RemoteRoot -notmatch '^[A-Za-z0-9_./-]+$') { Fail "Invalid remote root: $RemoteRoot" }

$url = if ($Version -eq "latest") { "$Repository/releases/latest/download" } else {
    if ($Version -notmatch '^v') { $Version = "v$Version" }
    "$Repository/releases/download/$Version"
}

if (-not $Quiet) {
    Write-Host ""
    Write-Host "Luna installer " -ForegroundColor White -NoNewline
    Write-Host ("version {0}" -f $Version) -ForegroundColor DarkGray
    Write-Host ("   {0}" -f $url) -ForegroundColor DarkGray
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ("luna-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    Write-Stage "Проверяем окружение"
    Write-Detail ("temp: {0}" -f $temp)
    Write-StageDone

    Write-Stage "Подключаемся к станции"
    $devices = @((& adb devices) | Where-Object { $_ -match '^([^\s]+)\s+device$' } | ForEach-Object { $Matches[1] })
    if (-not $Serial) {
        if ($devices.Count -eq 0) { Fail "No ADB station found." }
        if ($devices.Count -eq 1) { $Serial = $devices[0] }
        else {
            for ($i = 0; $i -lt $devices.Count; $i++) { Write-Host ("   {0}) {1}" -f ($i + 1), $devices[$i]) }
            $choice = [int](Read-Host "   выберите станцию")
            if ($choice -lt 1 -or $choice -gt $devices.Count) { Fail "Invalid station number: $choice" }
            $Serial = $devices[$choice - 1]
        }
    }
    if ((& adb -s $Serial get-state 2>$null) -ne "device") { Fail "ADB station is offline: $Serial" }
    Write-Detail ("station: {0}" -f $Serial)
    Write-StageDone

    if ($Uninstall) {
        Write-Stage "Удаляем Luna"
        $remoteText = @'
set -eu
root="$LUNA_ROOT"; init=/etc/init.d/S99luna
if [ -x "$init" ]; then "$init" stop || true; fi
rm -f "$init" "$root/bin/luna" "$root/bin/luna.previous" "$root/luna.pid"
echo "Luna removed"
'@
        $remoteText = ($remoteText -replace "`r`n", "`n") + "`n"
        [IO.File]::WriteAllText("$temp/remote.sh", $remoteText, [Text.Encoding]::ASCII)
        Get-Content "$temp/remote.sh" -Raw | & adb -s $Serial shell "LUNA_ROOT='$RemoteRoot' sh -s"
        if ($LASTEXITCODE -ne 0) { Fail "Remote uninstall failed" }
        Write-StageDone
        Write-Progress -Id 1 -Activity "Luna installer" -Completed
        Write-Host ""
        Write-Host ("Luna removed from {0}" -f $Serial) -ForegroundColor White
        return
    }

    Write-Stage "Скачиваем метаданные релиза"
    Get-RemoteFile "$url/manifest.json" "$temp/manifest.json" "manifest.json"
    Get-RemoteFile "$url/SHA256SUMS" "$temp/SHA256SUMS" "SHA256SUMS"
    $manifest = Get-Content "$temp/manifest.json" -Raw | ConvertFrom-Json
    Write-StageDone

    Write-Stage "Определяем станцию"
    if (-not [string]::IsNullOrWhiteSpace($Platform)) {
        $targetPlatform = $Platform.Trim()
        Write-Detail ("explicit platform: {0}" -f $targetPlatform)
    } else {
        $targetPlatform = Get-DetectedDtPlatform
        if ($targetPlatform) {
            Write-Detail ("platform from device-tree: {0}" -f $targetPlatform)
        } else {
            $targetPlatform = (Invoke-AdbShell 'uname -n').Trim()
            Write-Detail ("station hostname: {0}" -f $targetPlatform)
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
    if ($found.Count -ne 1) { Fail "Unsupported or ambiguous station. Target: $targetPlatform" }
    $entry = $found[0]
    $asset = $entry.asset
    Write-Detail ("model: {0} ({1})" -f $entry.device, $entry.codename)
    Write-StageDone

    Write-Stage "Скачиваем $asset"
    Get-RemoteFile "$url/$asset" "$temp/$asset" $asset
    Write-StageDone

    Write-Stage "Проверяем контрольную сумму"
    $sumLine = Get-Content "$temp/SHA256SUMS" | Where-Object { $_ -match "\s$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $sumLine) { Fail "No checksum found for $asset" }
    $expected = ($sumLine -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash "$temp/$asset" -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { Fail "Checksum mismatch for $asset" }
    Write-Detail ("sha256: {0}" -f $expected)
    Write-StageDone

    Write-Stage "Устанавливаем на станцию"
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
    start-stop-daemon --start --background --make-pidfile --pidfile "$pidfile" --startas "$binary" -- --listen __LUNA_LISTEN__ --data-dir "$root/data"
  else
    nohup "$binary" --listen __LUNA_LISTEN__ --data-dir "$root/data" >"$logfile" 2>&1 </dev/null & echo $! >"$pidfile"
  fi ;;
stop) [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true; rm -f "$pidfile" ;;
restart) "$0" stop; "$0" start ;;
*) exit 2 ;;
esac
'@
    $remoteText = @'
set -eu
root="$LUNA_ROOT"; binary="$root/bin/luna"; staged="$root/bin/luna.new"; pidfile="$root/luna.pid"; init=/etc/init.d/S99luna

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
    $remoteText = (($remoteText.Replace("__INIT__", $init.Replace("/data/quazar/luna", $RemoteRoot)).Replace("__LUNA_LISTEN__", $Listen)) -replace "`r`n", "`n") + "`n"
    $bootHook = if ($NoBootHook) { "0" } else { "1" }
    Invoke-AdbShell "mkdir -p '$RemoteRoot/bin'" | Out-Null
    $size = (Get-Item "$temp/$asset").Length
    Write-Detail ("upload {0} to {1}" -f (Format-Size $size), $RemoteRoot)
    [IO.File]::WriteAllText("$temp/remote.sh", $remoteText, [Text.Encoding]::ASCII)
    & adb -s $Serial push "$temp/$asset" "$RemoteRoot/bin/luna.new" | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "adb push failed" }
    Get-Content "$temp/remote.sh" -Raw | & adb -s $Serial shell "LUNA_ROOT='$RemoteRoot' LUNA_LISTEN='$Listen' LUNA_BOOT_HOOK='$bootHook' sh -s"
    if ($LASTEXITCODE -ne 0) { Fail "Remote installation failed" }
    Write-StageDone

    Write-Progress -Id 1 -Activity "Luna installer" -Completed
    $total = (Get-Date) - $script:InstallStarted
    Write-Host ""
    Write-Host "Luna installed " -ForegroundColor White -NoNewline
    Write-Host ("({0})" -f (Format-Duration $total)) -ForegroundColor DarkGray
    Write-Host ("   station: {0} ({1})" -f $entry.device, $entry.codename)
    Write-Host ("   version: {0}" -f $Version)
    Write-Host ("   API:     http://{0}/api" -f $Listen)
    Write-Host ("   data:    {0}/data" -f $RemoteRoot)
    Write-Host ("   log:     {0}/luna.log" -f $RemoteRoot)
    Write-Host ""
} catch {
    Write-Progress -Id 1 -Activity "Luna installer" -Completed
    Write-Host ""
    Write-Host ("error: {0}" -f $_.Exception.Message) -ForegroundColor Red
    exit 1
} finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
