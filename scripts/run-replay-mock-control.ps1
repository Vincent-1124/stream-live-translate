# Control experiment: same replay, but with the `mock` provider so the pipeline is
# exercised end-to-end WITHOUT TLS. If subtitles appear here, the TLS credential
# error is the only thing standing between the replay and a real transcript; if
# they do not, something in the ingest/gate/drain path is still broken.
#
# The real config is left untouched: a separate candidate config is written next
# to it and passed via --config.
$ErrorActionPreference = "Continue"

$repo = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$root = Split-Path -Parent (Split-Path -Parent $repo)
$cc = Join-Path $root "candidate-control"
$exe = Join-Path $cc "stream-live-translate.exe"
$log = Join-Path $cc "engine.mock.log"
$wav = Join-Path $env:TEMP "slt-replay-0454-11s.wav"
$replayScript = Join-Path $repo "scripts\send-replay-pcm.ps1"
$ff = (Get-ChildItem (Join-Path $env:APPDATA "Eagle\Plugins\ffmpeg-win-x64\ffmpeg.exe") -ErrorAction SilentlyContinue | Select-Object -First 1).FullName

function Say($m) { Write-Host $m }
function Status() {
    try { return (Invoke-WebRequest "http://127.0.0.1:8797/api/status" -TimeoutSec 6 -UseBasicParsing).Content }
    catch { return "API_ERROR: $($_.Exception.Message)" }
}
function Subs() {
    try { return (Invoke-WebRequest "http://127.0.0.1:8797/api/subtitles" -TimeoutSec 6 -UseBasicParsing).Content }
    catch { return "API_ERROR: $($_.Exception.Message)" }
}
function Utf8Fix([string]$s) {
    if (-not $s) { return $s }
    return [Text.Encoding]::UTF8.GetString([Text.Encoding]::GetEncoding(28591).GetBytes($s))
}

Get-Process -Name "stream-live-translate" -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -like "*candidate-control*" } |
    ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
Start-Sleep -Seconds 1

# Mock-provider config: take the real one and swap only the provider line, so
# every other setting (audio mode, ingest port, filter) stays identical.
$src = Get-Content (Join-Path $cc "config.toml") -Raw -Encoding UTF8
$mockCfg = Join-Path $cc "config.mock.toml"
$src2 = $src -replace 'provider = "[^"]*"', 'provider = "mock"'
Set-Content -LiteralPath $mockCfg -Value $src2 -Encoding UTF8 -NoNewline
Say "=== mock config written: $mockCfg (real config.toml untouched) ==="
($src2 -split "`n" | Select-String -Pattern '^provider' ) | ForEach-Object { "  " + $_.Line.Trim() }

Remove-Item $log -Force -ErrorAction SilentlyContinue
$args = "--config `"$mockCfg`" --host 127.0.0.1 --port 8797 --headless --ingest-port 8798"
$p = Start-Process -FilePath "cmd.exe" -ArgumentList @("/c", "$exe $args > `"$log`" 2>&1") -WorkingDirectory $cc -WindowStyle Hidden -PassThru
$owner = $null
for ($i = 1; $i -le 30; $i++) {
    Start-Sleep -Milliseconds 500
    $l = netstat -ano | Select-String -Pattern ":8797\s+0\.0\.0\.0:0\s+LISTENING" | Select-Object -First 1
    if ($l) { $owner = ($l.Line -split '\s+')[-1]; break }
}
Say "  8797 owner pid = $owner"
Say ("  baseline status = " + (Utf8Fix (Status)))

Say ""
Say "=== replay: 11 s real time ==="
$sw = [Diagnostics.Stopwatch]::StartNew()
$r = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $replayScript -InputPath $wav -Start "00:00:00" -DurationSeconds 11 -Port 8798 -Ffmpeg $ff -MaxSeconds 60 2>&1
$sw.Stop()
$r | Where-Object { $_ -match "audio_sent_s|elapsed_s|realtime_factor|realtime_ok|sent_pcm_bytes" } | ForEach-Object { "  " + $_.ToString().Trim() }
Say "  wall = $([math]::Round($sw.Elapsed.TotalSeconds,2)) s"

Say ""
Say "=== poll subtitles ==="
for ($t = 3; $t -le 30; $t += 3) {
    Start-Sleep -Seconds 3
    $s = Utf8Fix (Subs)
    $hist = if ($s -match '"history":\[(.*?)\]') { $Matches[1] } else { "" }
    $n = if ($hist) { ([regex]::Matches($hist, '"id"')).Count } else { 0 }
    Say "  t+${t}s history_count=$n"
    if ($n -gt 0) { Say "    -> got subtitles"; break }
}

Say ""
Say "=== FINAL ==="
Say ("status    = " + (Utf8Fix (Status)))
Say ("subtitles = " + (Utf8Fix (Subs)))
$txt = ""
try { $txt = (Invoke-WebRequest "http://127.0.0.1:8797/api/recordings/export?format=txt" -TimeoutSec 6 -UseBasicParsing).Content } catch { $txt = "API_ERROR" }
Say ("export txt= " + (Utf8Fix $txt))

Say ""
Say "=== JSONL (newest) ==="
Get-ChildItem (Join-Path $cc "recordings") -Force | Sort-Object LastWriteTime -Descending |
    Select-Object -First 2 | ForEach-Object {
        Say ("  " + $_.Name + "  " + $_.Length + " B  " + $_.LastWriteTime.ToString("HH:mm:ss"))
        if ($_.Length -gt 0) { Get-Content -LiteralPath $_.FullName -Encoding UTF8 | ForEach-Object { "    | " + $_ } }
    }

Say ""
Say "=== ENGINE LOG (full) ==="
if (Test-Path $log) { Get-Content $log -Encoding UTF8 | ForEach-Object { "  " + $_ } } else { Say "  (none)" }
Say ""
Say "=== DONE ==="
