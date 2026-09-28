# Installs the Windows OBS plugin from an extracted release package.
#
# The release package keeps the OBS 32-compatible bin/64bit layout.  OBS 33
# introduced a new layout with the module DLL next to data/.  Install both DLL
# locations so the same package works during that transition; OBS skips the
# duplicate after loading the preferred location.
[CmdletBinding()]
param(
    [string]$DestinationRoot = "",
    [switch]$SkipElevation,
    # Used by CI and unattended verification. Interactive installs always show
    # a final Windows dialog in addition to keeping the console open.
    [switch]$NoDialog
)

$ErrorActionPreference = "Stop"
$pluginName = "stream-live-translate"
$sourceRoot = $PSScriptRoot

function Show-ResultDialog {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [Parameter(Mandatory = $true)][string]$Title,
        [ValidateSet("Information", "Error")][string]$Icon
    )
    if ($NoDialog) { return }
    try {
        # WScript.Shell is present on supported Windows versions and does not
        # depend on the PowerShell runspace being STA. A zero timeout keeps the
        # result visible until the user acknowledges it.
        $popupIcon = if ($Icon -eq "Error") { 16 } else { 64 }
        $shell = New-Object -ComObject WScript.Shell
        [void]$shell.Popup($Message, 0, $Title, $popupIcon)
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
        return
    } catch {
        # Fall through to the .NET dialog on systems where Windows Script Host
        # has been disabled by policy.
    }
    try {
        Add-Type -AssemblyName PresentationFramework
        $messageBoxIcon = if ($Icon -eq "Error") {
            [System.Windows.MessageBoxImage]::Error
        } else {
            [System.Windows.MessageBoxImage]::Information
        }
        [void][System.Windows.MessageBox]::Show(
            $Message,
            $Title,
            [System.Windows.MessageBoxButton]::OK,
            $messageBoxIcon)
    } catch {
        # The console remains open through 双击安装.cmd, so even a Windows
        # installation without PresentationFramework still exposes the result.
    }
}

function Fail([string]$Message) {
    Write-Host "`n安装失败：$Message" -ForegroundColor Red
    Show-ResultDialog -Message "安装失败。`n`n$Message`n`n请根据提示处理后重新运行安装程序。" `
        -Title "Stream Live Translate - 安装失败" -Icon Error
    exit 1
}

function Assert-File([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Fail "发布包不完整，缺少文件：$Path"
    }
}

function Get-Sha256([string]$Path) {
    # Do not depend on Get-FileHash. Windows PowerShell normally autoloads it
    # from Microsoft.PowerShell.Utility, but inherited PSModulePath values can
    # make that module unavailable when the installer is launched by another
    # host. The .NET implementation works on every supported Windows version.
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            return (($sha.ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) -join "")
        } finally {
            $sha.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

$sourceDll = Join-Path $sourceRoot "bin\64bit\$pluginName.dll"
$sourceExe = Join-Path $sourceRoot "data\engine\$pluginName.exe"
$sourceLocale = Join-Path $sourceRoot "data\locale"
Assert-File $sourceDll
Assert-File $sourceExe
Assert-File (Join-Path $sourceLocale "en-US.ini")
Assert-File (Join-Path $sourceLocale "zh-CN.ini")

if (Get-Process -Name obs64 -ErrorAction SilentlyContinue) {
    Fail "请先完全退出 OBS（包括系统托盘中的 OBS），再重新运行本安装程序。"
}

if (-not $DestinationRoot) {
    $DestinationRoot = Join-Path $env:ProgramData "obs-studio\plugins"

    if (-not $SkipElevation) {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = New-Object Security.Principal.WindowsPrincipal($identity)
        $isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if (-not $isAdmin) {
            Write-Host "正在请求 Windows 管理员权限以安装 OBS 插件……" -ForegroundColor Cyan
            $escapedScript = $PSCommandPath.Replace('"', '""')
            $dialogArgument = if ($NoDialog) { " -NoDialog" } else { "" }
            $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$escapedScript`"$dialogArgument"
            try {
                $process = Start-Process -FilePath "powershell.exe" -Verb RunAs `
                    -ArgumentList $arguments -Wait -PassThru
                exit $process.ExitCode
            } catch {
                Fail "未获得管理员权限，无法写入 $DestinationRoot。"
            }
        }
    }
}

$destinationRootFull = [IO.Path]::GetFullPath($DestinationRoot)
$targetRoot = Join-Path $destinationRootFull $pluginName
$targetLegacyBin = Join-Path $targetRoot "bin\64bit"
$targetEngine = Join-Path $targetRoot "data\engine"
$targetLocale = Join-Path $targetRoot "data\locale"

Write-Host "`n正在安装 Stream Live Translate……" -ForegroundColor Cyan
Write-Host "目标位置：$targetRoot"

try {
    New-Item -ItemType Directory -Force -Path $targetLegacyBin, $targetEngine, $targetLocale | Out-Null

    # OBS 33+ preferred module layout.
    Copy-Item -LiteralPath $sourceDll -Destination (Join-Path $targetRoot "$pluginName.dll") -Force
    # OBS 32 and earlier ProgramData layout.
    Copy-Item -LiteralPath $sourceDll -Destination (Join-Path $targetLegacyBin "$pluginName.dll") -Force
    Copy-Item -LiteralPath $sourceExe -Destination (Join-Path $targetEngine "$pluginName.exe") -Force
    Copy-Item -LiteralPath (Join-Path $sourceLocale "en-US.ini") -Destination $targetLocale -Force
    Copy-Item -LiteralPath (Join-Path $sourceLocale "zh-CN.ini") -Destination $targetLocale -Force
} catch {
    Fail $_.Exception.Message
}

$checks = @(
    @($sourceDll, (Join-Path $targetRoot "$pluginName.dll")),
    @($sourceDll, (Join-Path $targetLegacyBin "$pluginName.dll")),
    @($sourceExe, (Join-Path $targetEngine "$pluginName.exe")),
    @((Join-Path $sourceLocale "en-US.ini"), (Join-Path $targetLocale "en-US.ini")),
    @((Join-Path $sourceLocale "zh-CN.ini"), (Join-Path $targetLocale "zh-CN.ini"))
)

foreach ($pair in $checks) {
    $sourceHash = Get-Sha256 $pair[0]
    $targetHash = Get-Sha256 $pair[1]
    if ($sourceHash -ne $targetHash) {
        Fail "安装后校验失败：$($pair[1])"
    }
}

Write-Host "`n安装成功。" -ForegroundColor Green
Write-Host "1. 重新打开 OBS。"
Write-Host "2. 在有声音的来源上打开“滤镜”。"
Write-Host "3. 点击 +，确认可以看到“实时字幕捕获”。"
Write-Host "`n本次更新只替换插件程序；已有的 Key、配置、录音和日志不会被删除。"

Show-ResultDialog `
    -Message "插件安装成功。`n`n安装位置：`n$targetRoot`n`n现在可以重新打开 OBS，在有声音的来源上添加「实时字幕捕获」滤镜。" `
    -Title "Stream Live Translate - 安装成功" -Icon Information
