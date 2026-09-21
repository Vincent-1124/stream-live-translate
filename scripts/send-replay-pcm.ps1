param(
    [Parameter(Mandatory = $true)][string]$InputPath,
    [Parameter(Mandatory = $true)][string]$Start,
    [Parameter(Mandatory = $true)][int]$DurationSeconds,
    [int]$Port = 8798,
    [string]$Ffmpeg = "ffmpeg"
)

$ErrorActionPreference = "Stop"
if (-not (Test-Path -LiteralPath $InputPath -PathType Leaf)) { throw "找不到录播：$InputPath" }

$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $Ffmpeg
$psi.Arguments = "-hide_banner -nostdin -ss $Start -t $DurationSeconds -i `"$InputPath`" -map 0:a:0 -ac 1 -ar 16000 -f s16le pipe:1"
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$process = [Diagnostics.Process]::new()
$process.StartInfo = $psi
if (-not $process.Start()) { throw "无法启动 ffmpeg" }

$client = [Net.Sockets.TcpClient]::new("127.0.0.1", $Port)
$stream = $client.GetStream()
try {
    # SLTA + 16 kHz + mono signed 16-bit PCM, matching the OBS filter protocol.
    $header = [byte[]](0x53,0x4c,0x54,0x41,0x80,0x3e,0x00,0x00,0x00,0x00,0x00,0x00)
    $stream.Write($header, 0, $header.Length)
    $buffer = New-Object byte[] 640 # 20 ms at 16 kHz mono s16le
    while (($read = $process.StandardOutput.BaseStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
        $stream.Write($buffer, 0, $read)
        Start-Sleep -Milliseconds 20
    }
    $stream.Flush()
} finally {
    $stream.Dispose(); $client.Dispose()
    if (-not $process.HasExited) { $process.Kill() }
    $process.Dispose()
}
