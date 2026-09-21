param(
    [Parameter(Mandatory = $true)]
    [string]$InputPath,
    [string]$OutputDir = "build\\replay-samples"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $InputPath -PathType Leaf)) {
    throw "录播文件不存在：$InputPath"
}
if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    throw "未找到 ffmpeg。安装后重试；本脚本不会上传或修改原始录播。"
}

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
    & ffmpeg -hide_banner -nostdin -y -ss $sample.Start -i $InputPath -t $sample.Duration `
        -map 0:a:0 -ac 1 -ar 16000 -c:a pcm_s16le $out
    if ($LASTEXITCODE -ne 0) { throw "提取失败：$($sample.Name)" }
    $hash = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash
    [pscustomobject]@{ sample = $sample.Name; start = $sample.Start; duration_s = $sample.Duration; file = $out; sha256 = $hash }
}
