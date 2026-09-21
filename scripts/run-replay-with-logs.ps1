# One-shot: start the engine WITH runtime logs, replay, then dump the log tail.
# This environment reaps orphaned children when the launching command exits, so
# start + replay + collect all have to happen inside one process lifetime.
#
# The engine is started via cmd.exe redirection rather than Start-Process
# -RedirectStandardOutput/-RedirectStandardError: on this machine the PowerShell
# redirection path made the engine exit right after binding, while a plain
# launch (and this cmd redirection) survives.
$ErrorActionPreference = "Continue"

$repo = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$root = Split-Path -Parent (Split-Path -Parent $repo)
$cc = Join-Path $root "candidate-control"
$exe = Join-Path $cc "stream-live-translate.exe"
$cfg = Join-Path $cc "config.toml"
$log = Join-Path $cc "engine.run.log"
$wav = Join-Path $env:TEMP "slt-replay-0454-11s.wav"
$replayScript = Join-Path $repo "scripts\send-replay-pcm.ps1"
$ff = (Get-ChildItem (Join-Path $env:APPDATA "Eagle\Plugins\ffmpeg-win-x64\ffmpeg.exe") -ErrorAction SilentlyContinue | Select-Object -First 1).FullName

function Say($m) { Write-Host $m }
function Status() {
    try { return (Invoke-WebRequest "http://127.0.0.1:8797/api/status" -TimeoutSec 6 -UseBasicParsing).Content }
    catch { return "API_ERROR" }
}
# The status JSON is valid UTF-8 but Invoke-WebRequest hands back a string that
# was decoded as Latin-1, so non-ASCII fields look like mojibake. Re-decode so the
# Chinese error text is readable in the evidence.
function Utf8Fix([string]$s) {
    if (-not $s) { return $s }
    return [Text.Encoding]::UTF8.GetString([Text.Encoding]::GetEncoding(28591).GetBytes($s))
}

Get-Process -Name "stream-live-translate" -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -like "*candidate-control*" } |
    ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
Start-Sleep -Seconds 1
Remove-Item $log -Force -ErrorAction SilentlyContinue

Say "=== engine + log ==="
$args = "--config `"$cfg`" --host 127.0.0.1 --port 8797 --headless --ingest-port 8798"
$p = Start-Process -FilePath "cmd.exe" -ArgumentList @("/c", "$exe $args > `"$log`" 2>&1") -WorkingDirectory $cc -WindowStyle Hidden -PassThru
Say "  wrapper pid = $($p.Id)"

$owner = $null
for ($i = 1; $i -le 30; $i++) {
    Start-Sleep -Milliseconds 500
    $l = netstat -ano | Select-String -Pattern ":8797\s+0\.0\.0\.0:0\s+LISTENING" | Select-Object -First 1
    if ($l) { $owner = ($l.Line -split '\s+')[-1]; break }
}
Say "  8797 owner pid = $owner  (after $([math]::Round($i*0.5,1)) s)"
Say "  status = $(Utf8Fix (Status))"

Say ""
Say "=== replay: 11 s from 00:04:54 (real time) ==="
$sw = [Diagnostics.Stopwatch]::StartNew()
$r = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $replayScript -InputPath $wav -Start "00:00:00" -DurationSeconds 11 -Port 8798 -Ffmpeg $ff -MaxSeconds 60 2>&1
$sw.Stop()
$r | Where-Object { $_ -match "audio_sent_s|elapsed_s|realtime_factor|realtime_ok|sent_pcm_bytes|lateness_max|mode" } | ForEach-Object { "  " + $_.ToString().Trim() }
Say "  wall = $([math]::Round($sw.Elapsed.TotalSeconds,2)) s"

Say ""
Say "=== status right after replay ==="
Say "  $(Utf8Fix (Status))"
for ($t = 5; $t -le 40; $t += 5) {
    Start-Sleep -Seconds 5
    $st = Utf8Fix (Status)
    $err = if ($st -match '"last_error":"([^"]*)"') { $Matches[1] } else { "?" }
    $llm = if ($st -match '"llm_connected":(\w+)') { $Matches[1] } else { "?" }
    Say "  t+${t}s llm_connected=$llm last_error=$err"
}

Say ""
Say "=== final evidence ==="
Say "--- status ---"
Say ("  " + (Utf8Fix (Status)))
Say "--- subtitles ---"
Say ("  " + (Utf8Fix (try { (Invoke-WebRequest "http://127.0.0.1:8797/api/subtitles" -TimeoutSec 6 -UseBasicParsing).Content } catch { "API_ERROR" })))

Say ""
Say "=== ENGINE LOG (full) ==="
if (Test-Path $log) {
    Get-Content $log -Encoding UTF8 | ForEach-Object { "  " + $_ }
} else { Say "  (no log file)" }

Say ""
Say "=== JSONL files touched in this run (by mtime desc, first 3) ==="
Get-ChildItem (Join-Path $cc "recordings") -Force | Sort-Object LastWriteTime -Descending |
    Select-Object -First 3 | ForEach-Object {
        Say ("  " + $_.Name + "  " + $_.Length + " B  " + $_.LastWriteTime.ToString("HH:mm:ss"))
        if ($_.Length -gt 0) { Get-Content -LiteralPath $_.FullName -Encoding UTF8 | ForEach-Object { "    | " + $_ } }
    }

Say ""
Say "=== DONE ==="
