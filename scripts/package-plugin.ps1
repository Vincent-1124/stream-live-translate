# ---------------------------------------------------------------------------
# package-plugin.ps1 — build the OBS plugin package for Windows x64.
#
# Output: release/stream-live-translate-obs-win-x64-<version>.zip
#
# Prerequisites (all free):
#   * Rust toolchain (rustup, stable, MSVC ABI)
#   * Visual Studio Build Tools with the C++ workload (cl, lib, dumpbin)
#   * CMake 3.16+
#   * Git
#   * OBS Studio installed somewhere (for obs.dll), or the official release
#     zip will be downloaded automatically.
#
# The script does NOT build OBS itself: it only needs libobs *headers*
# (shallow obs-studio clone) plus an import library generated from the
# installed obs.dll. Real symbols resolve at runtime against OBS.
# ---------------------------------------------------------------------------
param(
    [string]$ObsVersion = "30.2.3",
    [string]$ObsInstallDir = "",
    [string]$WorkDir = "build\plugin-sdk",
    [switch]$SkipEngine,
    # Rewrite the ?v= cache-busting strings in admin/index.html and
    # overlay/index.html to the canonical Cargo.toml version. Off by default
    # so a packaging run never silently edits files owned by other agents;
    # without it the run fails loudly with the exact drift instead.
    [switch]$FixAssetVersions
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# MSVC's linker can inherit a non-Unicode-compatible user Temp path.  Keep
# its response files under the build directory for this packaging run.
$linkTemp = Join-Path $root "build\tmp"
New-Item -ItemType Directory -Force -Path $linkTemp | Out-Null
$env:TEMP = $linkTemp
$env:TMP = $linkTemp

# A relative WorkDir must be anchored at the repo root: CMake resolves a
# relative LIBOBS_INCLUDE_DIR against the plugin source directory, not our
# working directory, which would make obs-module.h unfindable.
if (-not [System.IO.Path]::IsPathRooted($WorkDir)) {
    $WorkDir = Join-Path $root $WorkDir
}

function Step([string]$msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }

# Native tools (cargo, git, cmake, dumpbin, lib) routinely write progress and
# warnings to stderr.  Under $ErrorActionPreference = "Stop", Windows
# PowerShell 5.1 promotes those stderr lines to *terminating*
# NativeCommandErrors as soon as anything captures the stream — e.g. running
# this script through `script.ps1 2>&1 | Tee-Object log`, or any CI wrapper.
# That turned a harmless "unused manifest key" cargo warning into a build
# failure.  Run native commands with stderr merged and error promotion off,
# then fail explicitly on a non-zero exit code.  Output is echoed to the host
# and also returned, so callers may parse stdout.
function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Command,
        [string]$What = "native command"
    )
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $captured = @()
    try {
        $captured = & $Command 2>&1
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    foreach ($line in $captured) { Write-Host $line }
    if ($code -ne 0) {
        throw "$What exited with code $code"
    }
    return $captured
}

# Version straight from Cargo.toml.
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Step "Packaging Stream Live Translate OBS plugin v$version (Windows x64)"

# --- 0a. Version consistency ------------------------------------------------
# Cargo.toml is the single source of truth. plugin/version.h must match it
# (plugin/CMakeLists.txt aborts otherwise) and the ?v= cache-busting strings
# must not advertise a different release. Fail here, before anything is built,
# rather than shipping a package whose asset URLs claim another version.
$syncArgs = @{}
if ($FixAssetVersions) { $syncArgs["Fix"] = $true }
Step "Checking version consistency (canonical: Cargo.toml v$version)"
$versionSync = Join-Path $PSScriptRoot "sync-version.ps1"
& $versionSync @syncArgs
if ($LASTEXITCODE -ne 0) {
    throw ("Version consistency check failed. Cargo.toml says $version. " +
        "Re-run with -FixAssetVersions to rewrite the admin/overlay ?v= strings, " +
        "or apply the edits listed in docs/BUILD.md by hand.")
}

# --- 0. Sanity: MSVC toolchain on PATH ------------------------------------
foreach ($exe in "lib", "dumpbin", "cl") {
    if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) {
        throw "$exe not on PATH. Run this from a 'Developer PowerShell for VS' or call Import-Module ...Microsoft.VisualStudio.DevShell.dll; Enter-VsDevShell first."
    }
}

# --- 1. Engine -------------------------------------------------------------
if (-not $SkipEngine) {
    Step "Building Rust engine (release)"
    Push-Location $root
    Invoke-Native { cargo build --release } "cargo build --release" | Out-Null
    Pop-Location
}
$engineExe = "$root\target\release\stream-live-translate.exe"
if (-not (Test-Path $engineExe)) { throw "engine binary missing: $engineExe" }

# --- 2. OBS SDK: headers ---------------------------------------------------
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$obsSrc = Join-Path $WorkDir "obs-studio"
if (-not (Test-Path "$obsSrc\libobs\obs-module.h")) {
    Step "Fetching libobs headers (obs-studio $ObsVersion, shallow clone)"
    Invoke-Native { git clone --depth 1 --branch $ObsVersion https://github.com/obsproject/obs-studio $obsSrc } "git clone obs-studio" | Out-Null
}

# --- 3. OBS SDK: obs.dll + import library ----------------------------------
$obsDll = ""
if ($ObsInstallDir -and (Test-Path "$ObsInstallDir\bin\64bit\obs.dll")) {
    $obsDll = "$ObsInstallDir\bin\64bit\obs.dll"
} else {
    foreach ($d in "$env:ProgramFiles\obs-studio", "${env:ProgramFiles(x86)}\obs-studio") {
        if (Test-Path "$d\bin\64bit\obs.dll") { $obsDll = "$d\bin\64bit\obs.dll"; break }
    }
}
if (-not $obsDll) {
    Step "OBS install not found; downloading official OBS-Studio-$ObsVersion zip"
    $zip = Join-Path $WorkDir "obs-full.zip"
    if (-not (Test-Path $zip)) {
        Invoke-WebRequest -Uri "https://github.com/obsproject/obs-studio/releases/download/$ObsVersion/OBS-Studio-$ObsVersion-Windows.zip" -OutFile $zip
    }
    $obsFull = Join-Path $WorkDir "obs-full"
    if (-not (Test-Path $obsFull)) {
        # Developer PowerShell can fail to load Microsoft.PowerShell.Archive.
        # The .NET extractor is available in both normal and VS shells.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $obsFull)
    }
    $obsDll = (Get-ChildItem "$WorkDir\obs-full" -Recurse -Filter obs.dll |
        Select-Object -First 1).FullName
}
Step "Using obs.dll: $obsDll"

$sdkBin = Join-Path $WorkDir "sdk-bin"
New-Item -ItemType Directory -Force -Path $sdkBin | Out-Null
$defFile = Join-Path $sdkBin "obs.def"
$obsLib = Join-Path $sdkBin "obs.lib"
if (-not (Test-Path $obsLib)) {
    Step "Generating import library obs.lib from obs.dll exports"
    $exports = Invoke-Native { dumpbin /exports $obsDll } "dumpbin /exports" |
        Where-Object { $_ -is [string] } |
        Where-Object { $_ -match '^\s+\d+\s+[0-9A-F]+\s+[0-9A-F]+\s+(\S+)' } |
        ForEach-Object { $Matches[1] } |
        Where-Object { $_ -notmatch '^@' -and $_ -notmatch '\.dll$' } |
        Sort-Object -Unique
    if (-not $exports) { throw "dumpbin produced no exports; wrong obs.dll?" }
    Set-Content -Path $defFile -Value (@("LIBRARY obs", "EXPORTS") + $exports) -Encoding ASCII
    Invoke-Native { lib /nologo /machine:x64 "/def:$defFile" "/out:$obsLib" } "lib /def" | Out-Null
    if (-not (Test-Path $obsLib)) { throw "failed to create obs.lib" }
}

# --- 4. Build the plugin ---------------------------------------------------
Step "Building plugin (CMake/MSVC)"
$pluginBuild = Join-Path $WorkDir "..\plugin-build-win"
Invoke-Native { cmake -S "$root\plugin" -B $pluginBuild `
    -DCMAKE_BUILD_TYPE=Release `
    "-DLIBOBS_INCLUDE_DIR=$obsSrc\libobs" `
    "-DOBS_IMPORT_LIB=$obsLib" } "cmake configure" | Out-Null
Invoke-Native { cmake --build $pluginBuild --config Release } "cmake --build" | Out-Null
$dll = Get-ChildItem $pluginBuild -Recurse -Filter "stream-live-translate.dll" |
    Select-Object -First 1
if (-not $dll) { throw "plugin dll not found" }

# --- 5. Assemble + zip ------------------------------------------------------
Step "Assembling plugin folder"
$stage = Join-Path $WorkDir "..\stage-win"
$pkgRoot = Join-Path $stage "stream-live-translate"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path "$pkgRoot\bin\64bit" | Out-Null
New-Item -ItemType Directory -Force -Path "$pkgRoot\data\locale" | Out-Null
New-Item -ItemType Directory -Force -Path "$pkgRoot\data\engine" | Out-Null
Copy-Item $dll.FullName "$pkgRoot\bin\64bit\"
Copy-Item "$root\plugin\locale\*.ini" "$pkgRoot\data\locale\"
Copy-Item $engineExe "$pkgRoot\data\engine\"
$installerScript = "$root\scripts\install-plugin.ps1"
$installerBytes = [System.IO.File]::ReadAllBytes($installerScript)
if ($installerBytes.Length -lt 3 -or
    $installerBytes[0] -ne 0xEF -or
    $installerBytes[1] -ne 0xBB -or
    $installerBytes[2] -ne 0xBF) {
    throw "install-plugin.ps1 must be UTF-8 with BOM so Windows PowerShell 5.1 can parse its Chinese UI text"
}
Copy-Item $installerScript "$pkgRoot\install-plugin.ps1"
$installerCmd = "$root\scripts\install-plugin.cmd"
$installerCmdText = [System.IO.File]::ReadAllText($installerCmd, [System.Text.Encoding]::UTF8)
if ($installerCmdText -match '(?<!\r)\n') {
    throw "install-plugin.cmd must use Windows CRLF line endings so cmd.exe does not split commands incorrectly"
}
Copy-Item $installerCmd "$pkgRoot\双击安装.cmd"
$userGuides = @(Get-ChildItem "$root\docs" -Filter "*-Windows.md" -File)
if ($userGuides.Count -ne 1) { throw "expected one Windows user guide, found $($userGuides.Count)" }
Copy-Item $userGuides[0].FullName "$pkgRoot\README.md"

New-Item -ItemType Directory -Force -Path "$root\release" | Out-Null
$outZip = "$root\release\stream-live-translate-obs-win-x64-$version.zip"
if (Test-Path $outZip) { Remove-Item $outZip -Force }
# Compress-Archive on Windows PowerShell 5.1 stores entry names with backslash
# separators ("stream-live-translate\bin\64bit\..."), which is not valid per the
# ZIP spec (APPNOTE 4.4.17.1 mandates '/').  Windows Explorer tolerates it, but
# `unzip` on Linux/macOS, 7-Zip listings and any automated manifest check end up
# with one flat file whose name contains literal backslashes.  Write the archive
# ourselves so entry names are conformant.
Add-Type -AssemblyName System.IO.Compression.FileSystem
Add-Type -AssemblyName System.IO.Compression
# Use the *resolved* parent path: $pkgRoot is built from $WorkDir, which still
# contains a literal ".." segment, so its string length does not match the
# fully-resolved paths Get-ChildItem returns.  Slicing entry names off the
# unresolved string silently truncated the leading "stream-live-translate".
$zipBase = (Get-Item -LiteralPath $pkgRoot).Parent.FullName
$archive = [System.IO.Compression.ZipFile]::Open($outZip, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($file in (Get-ChildItem -LiteralPath $pkgRoot -Recurse -File | Sort-Object FullName)) {
        if (-not $file.FullName.StartsWith($zipBase, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to archive $($file.FullName): outside $zipBase"
        }
        $entryName = $file.FullName.Substring($zipBase.Length + 1).Replace('\', '/')
        [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, $file.FullName, $entryName,
            [System.IO.Compression.CompressionLevel]::Optimal)
    }
} finally {
    $archive.Dispose()
}
$hash = (Get-FileHash $outZip -Algorithm SHA256).Hash
Set-Content -Path "$outZip.sha256" -Value "$hash  $(Split-Path -Leaf $outZip)"

Step "Done: $outZip"
Write-Host "    SHA256: $hash"
Write-Host "    Install: extract the package, close OBS, then double-click"
Write-Host "    stream-live-translate\双击安装.cmd"
