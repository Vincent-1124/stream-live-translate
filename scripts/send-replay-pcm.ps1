param(
    [Parameter(Mandatory = $true)][string]$InputPath,
    [Parameter(Mandatory = $true)][string]$Start,
    [Parameter(Mandatory = $true)][int]$DurationSeconds,
    [int]$Port = 8798,
    [string]$Ffmpeg = "ffmpeg",
    [int]$MaxSeconds = 120,
    [switch]$Fast,
    [switch]$NoPacing
)

# Streams a slice of a recording into the engine's OBS-filter audio ingest as
# 16 kHz mono s16le PCM.
#
# NOTE ON ENCODING: this file is deliberately ASCII-only.  Windows PowerShell
# 5.1 (powershell.exe) decodes a BOM-less file as the ANSI code page, which is
# GBK/936 on a Chinese Windows install, so any non-ASCII literal in a .ps1 is
# read as mojibake and can break the parser.  Keep this script ASCII-only, and
# keep the UTF-8 BOM if you ever add non-ASCII text.
#
# ## Pacing is ON by default, on purpose
#
# `ffmpeg` decodes a file far faster than real time, but everything downstream is
# wall-clock driven: the cloud provider's endpointing, the VAD debounce, and the
# capture counters. A burst is therefore not "the same replay, just quicker":
#   * the engine's PCM channel holds only ~5 s of audio (PCM_CHANNEL_CAPACITY in
#     src/pipeline.rs) and the ingest path uses try_send, so a burst silently
#     DROPS frames once the channel is full -- the transcript comes back with
#     content missing and nothing reports it;
#   * Bailian's endpointing is time-based, so burst input makes the server
#     behave nothing like a live stream.
# Only real-time pacing supports timing or acceptance conclusions, so it is the
# default. -Fast / -NoPacing exist for transport smoke tests only.
#
# ## Why an absolute timeline
#
# The previous implementation slept a fixed 20 ms after every 20 ms chunk, which
# made the send rate "20 ms + I/O cost" per chunk: a nominal 11 s slice really
# took 17.36 s (~31.6 ms/chunk), i.e. the replay was stretched to 0.63x speed and
# no timing conclusion drawn from it was valid. (A local simulation of the same
# loop with 12 ms of I/O cost reproduces the shape: fixed sleep 2.36x wall time
# vs absolute timeline 1.00x.)
#
# Chunk N must be on the wire by `streamStart + N * chunkDuration`; the sleep is
# shortened (or skipped) by whatever the read/write already cost, so real time is
# a floor that I/O overhead cannot inflate. If a chunk cannot make its deadline
# the script reports it (lateness_*) rather than silently falling behind.

$ErrorActionPreference = "Stop"
if (-not (Test-Path -LiteralPath $InputPath -PathType Leaf)) { throw "recording not found: $InputPath" }
if (-not (Test-Path -LiteralPath $Ffmpeg -PathType Leaf) -and -not (Get-Command $Ffmpeg -ErrorAction SilentlyContinue)) {
    throw "ffmpeg not found: $Ffmpeg (pass -Ffmpeg with a full path)"
}

# 16 kHz mono s16le = 32000 bytes per second of audio.
$BytesPerSecond = 32000
$ChunkBytes = 640          # 20 ms

$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $Ffmpeg
# -vn drops video, so ffmpeg only decodes audio for the requested window.
$psi.Arguments = "-hide_banner -nostdin -ss $Start -t $DurationSeconds -i `"$InputPath`" -vn -map 0:a:0 -ac 1 -ar 16000 -f s16le pipe:1"
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$process = [Diagnostics.Process]::new()
$process.StartInfo = $psi
if (-not $process.Start()) { throw "could not start ffmpeg" }
$stderrTask = $process.StandardError.ReadToEndAsync()

$client = [Net.Sockets.TcpClient]::new("127.0.0.1", $Port)
$stream = $client.GetStream()
$clock = [Diagnostics.Stopwatch]::StartNew()
$sentBytes = 0
$chunks = 0
$behindMs = 0.0
$maxLatenessMs = 0.0
try {
    # SLTA + 16 kHz + mono signed 16-bit PCM, matching the OBS filter protocol.
    $header = [byte[]](0x53,0x4c,0x54,0x41,0x80,0x3e,0x00,0x00,0x00,0x00,0x00,0x00)
    $stream.Write($header, 0, $header.Length)
    $buffer = New-Object byte[] $ChunkBytes
    while (($read = $process.StandardOutput.BaseStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
        if ($clock.Elapsed.TotalSeconds -gt $MaxSeconds) { throw "replay exceeded $MaxSeconds s, stopped" }
        $stream.Write($buffer, 0, $read)
        $sentBytes += $read
        $chunks++
        if (-not $NoPacing) {
            # Deadline for the audio contained in everything sent so far.
            $deadline = $sentBytes / $BytesPerSecond
            while ($true) {
                $remaining = $deadline - $clock.Elapsed.TotalSeconds
                if ($remaining -le 0) { break }
                if ($remaining -gt 0.005) { Start-Sleep -Milliseconds ([int](($remaining - 0.002) * 1000)) }
                else { [Threading.Thread]::SpinWait(200) }
            }
            $lateness = ($clock.Elapsed.TotalSeconds - $deadline) * 1000
            if ($lateness -gt 0) {
                $behindMs += $lateness
                if ($lateness -gt $maxLatenessMs) { $maxLatenessMs = $lateness }
            }
        }
    }
    $stream.Flush()
} finally {
    $stream.Dispose(); $client.Dispose()
    if (-not $process.HasExited) { $process.Kill() }
    $process.WaitForExit()
    $stderrText = $stderrTask.GetAwaiter().GetResult()
    $process.Dispose()
}

$elapsed = $clock.Elapsed.TotalSeconds
$audioSeconds = $sentBytes / $BytesPerSecond
$factor = if ($audioSeconds -gt 0) { $elapsed / $audioSeconds } else { 1.0 }
$mode = if ($NoPacing) { "unpaced" } elseif ($Fast) { "fast (pacing disabled)" } else { "realtime" }

# A factor well under 1 means the audio went in faster than it was spoken, which
# invalidates any timing conclusion and risks silent frame drops in the engine.
# Say so loudly instead of letting a meaningless run look successful.
$warning = $null
if ($factor -lt 0.9) {
    $warning = ("NOT a real-time replay (realtime_factor={0}): the engine may silently drop audio on backpressure. " -f [math]::Round($factor, 3)) +
               "Do not use this run for timing/latency conclusions or acceptance; transport smoke test only."
    Write-Warning $warning
}
if ($maxLatenessMs -gt 250) {
    Write-Warning ("Replay fell behind its timeline: total {0} ms, worst single chunk {1} ms. Timing conclusions are unreliable." -f `
        [math]::Round($behindMs, 1), [math]::Round($maxLatenessMs, 1))
}

[pscustomobject]@{
    start                = $Start
    requested_s          = $DurationSeconds
    audio_sent_s         = [math]::Round($audioSeconds, 3)
    sent_pcm_bytes       = $sentBytes
    chunks               = $chunks
    elapsed_s            = [math]::Round($elapsed, 3)
    mode                 = $mode
    realtime_factor      = [math]::Round($factor, 3)
    realtime_ok          = ($factor -ge 0.9)
    chunks_per_second    = if ($elapsed -gt 0) { [math]::Round($chunks / $elapsed, 1) } else { $null }
    lateness_total_ms    = [math]::Round($behindMs, 1)
    lateness_max_ms      = [math]::Round($maxLatenessMs, 1)
    warning              = $warning
    ffmpeg_stderr        = $stderrText.Trim()
}
