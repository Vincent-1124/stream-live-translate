param(
    [Parameter(Mandatory = $true)]
    [string]$InputPath,
    [string]$OutputDir = "build\replay-samples",
    [string]$Ffmpeg = "ffmpeg",
    [switch]$Force
)

# Extracts the fixed replay samples used for latency/layout review as 16 kHz mono
# s16le WAV. Read-only with respect to the source recording; nothing is uploaded.
#
# NOTE ON ENCODING: this file is deliberately ASCII-only. Windows PowerShell 5.1
# (powershell.exe) decodes a BOM-less file as the ANSI code page, which is
# GBK/936 on a Chinese Windows install, so non-ASCII literals here were read as
# mojibake and broke the parser ("Unexpected token '}'"). Keep it ASCII-only, and
# keep a UTF-8 BOM if non-ASCII text is ever added.
#
# `-Ffmpeg` accepts a full path, because ffmpeg is frequently installed as part
# of another application and therefore missing from PATH.

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $InputPath -PathType Leaf)) {
    throw "recording not found: $InputPath"
}
$ffmpegExe = $Ffmpeg
if (-not (Test-Path -LiteralPath $ffmpegExe -PathType Leaf)) {
    $found = Get-Command $Ffmpeg -ErrorAction SilentlyContinue
    if (-not $found) {
        throw "ffmpeg not found: $Ffmpeg (pass -Ffmpeg with a full path). The source recording is never modified or uploaded."
    }
    $ffmpegExe = $found.Source
}

# The first two are the frames the user authorised for re-runs (11 s each); the
# longer ones are calibration material and are much larger on disk.
$samples = @(
    @{ Name = "layout-0454"; Start = "00:04:54"; Duration = 11 },
    @{ Name = "layout-2954"; Start = "00:29:54"; Duration = 11 },
    @{ Name = "normal-candidate"; Start = "00:02:00"; Duration = 300 },
    @{ Name = "quiet-candidate"; Start = "00:12:00"; Duration = 120 },
    @{ Name = "ambient-candidate"; Start = "00:20:00"; Duration = 180 },
    @{ Name = "long-sentence-candidate"; Start = "00:29:50"; Duration = 120 }
)

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
foreach ($sample in $samples) {
    $out = Join-Path $OutputDir ("{0}.wav" -f $sample.Name)
    if ((Test-Path -LiteralPath $out) -and -not $Force) {
        Write-Host ("skip (exists): {0}" -f $out)
    } else {
        & $ffmpegExe -hide_banner -nostdin -y -ss $sample.Start -i $InputPath -t $sample.Duration `
            -map 0:a:0 -ac 1 -ar 16000 -c:a pcm_s16le $out
        if ($LASTEXITCODE -ne 0) { throw "extract failed: $($sample.Name)" }
    }
    $hash = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash
    $size = (Get-Item -LiteralPath $out).Length
    [pscustomobject]@{
        sample     = $sample.Name
        start      = $sample.Start
        duration_s = $sample.Duration
        file       = $out
        bytes      = $size
        audio_s    = [math]::Round($size / 32000, 2)
        sha256     = $hash
    }
}
