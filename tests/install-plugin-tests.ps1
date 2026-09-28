$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path ([IO.Path]::GetTempPath()) ("slt-installer-test-" + [guid]::NewGuid().ToString("N"))
$package = Join-Path $fixture "package"
$destination = Join-Path $fixture "ProgramData\obs-studio\plugins"

try {
    New-Item -ItemType Directory -Force -Path `
        (Join-Path $package "bin\64bit"), `
        (Join-Path $package "data\engine"), `
        (Join-Path $package "data\locale") | Out-Null
    Set-Content -LiteralPath (Join-Path $package "bin\64bit\stream-live-translate.dll") -Value "test-dll"
    Set-Content -LiteralPath (Join-Path $package "data\engine\stream-live-translate.exe") -Value "test-exe"
    Set-Content -LiteralPath (Join-Path $package "data\locale\en-US.ini") -Value "test-en"
    Set-Content -LiteralPath (Join-Path $package "data\locale\zh-CN.ini") -Value "test-zh"
    Copy-Item -LiteralPath (Join-Path $root "scripts\install-plugin.ps1") -Destination $package
    $cmdInstaller = Join-Path $package "双击安装.cmd"
    Copy-Item -LiteralPath (Join-Path $root "scripts\install-plugin.cmd") -Destination $cmdInstaller

    $installerBytes = [IO.File]::ReadAllBytes((Join-Path $package "install-plugin.ps1"))
    if ($installerBytes.Length -lt 3 -or
        $installerBytes[0] -ne 0xEF -or
        $installerBytes[1] -ne 0xBB -or
        $installerBytes[2] -ne 0xBF) {
        throw "install-plugin.ps1 is not UTF-8 with BOM; Windows PowerShell 5.1 will misparse Chinese text"
    }
    $cmdText = [IO.File]::ReadAllText($cmdInstaller, [Text.Encoding]::UTF8)
    if ($cmdText -match '(?<!\r)\n') {
        throw "install-plugin.cmd does not use Windows CRLF line endings"
    }

    $preserved = Join-Path $destination "stream-live-translate\data\engine\config.toml"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $preserved) | Out-Null
    Set-Content -LiteralPath $preserved -Value "api_key = 'keep-me'"

    & (Join-Path $package "install-plugin.ps1") -DestinationRoot $destination -SkipElevation -NoDialog
    if (-not $?) { throw "installer failed" }

    $expected = @(
        "stream-live-translate\stream-live-translate.dll",
        "stream-live-translate\bin\64bit\stream-live-translate.dll",
        "stream-live-translate\data\engine\stream-live-translate.exe",
        "stream-live-translate\data\locale\en-US.ini",
        "stream-live-translate\data\locale\zh-CN.ini"
    )
    foreach ($relative in $expected) {
        if (-not (Test-Path -LiteralPath (Join-Path $destination $relative) -PathType Leaf)) {
            throw "missing installed file: $relative"
        }
    }
    if ((Get-Content -LiteralPath $preserved -Raw) -notmatch "keep-me") {
        throw "existing config.toml was overwritten"
    }

    # Exercise the same chain as a user's double click. The batch file invokes
    # Windows PowerShell 5.1 (`powershell.exe`), not the pwsh host running CI.
    # Redirect stdin so its final `pause` receives EOF and does not block.
    $cmdDestination = Join-Path $fixture "cmd-install\obs-studio\plugins"
    $previousTestRoot = $env:SLT_INSTALL_TEST_ROOT
    try {
        $env:SLT_INSTALL_TEST_ROOT = $cmdDestination
        & cmd.exe /d /c "call `"$cmdInstaller`" < nul"
        if ($LASTEXITCODE -ne 0) {
            throw "double-click CMD chain exited with code $LASTEXITCODE"
        }
    } finally {
        $env:SLT_INSTALL_TEST_ROOT = $previousTestRoot
    }
    if (-not (Test-Path -LiteralPath (Join-Path $cmdDestination "stream-live-translate\stream-live-translate.dll"))) {
        throw "double-click CMD chain did not install the OBS 33 layout"
    }

    Write-Host "Windows installer tests passed" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $fixture) {
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
}
