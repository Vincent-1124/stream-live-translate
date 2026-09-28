# ---------------------------------------------------------------------------
# sync-version.ps1 — make every derived version in this repo agree with the
# ONE authoritative version: the `version` field of the root Cargo.toml.
#
# What it touches (and why it is allowed to):
#   * admin/index.html, overlay/index.html  -> the `?v=<version>` cache-busting
#     query strings. Nothing else in the HTML is modified.
#   * dist/admin/index.html, dist/overlay/index.html -> only if `cargo build`
#     (build.rs) already synced them; refreshed here so a subsequent
#     `include_dir!` embed cannot pick up a stale asset URL.
#
# What it deliberately does NOT touch:
#   * plugin/version.h is reported, never rewritten. That keeps the version
#     header a reviewed artifact; plugin/CMakeLists.txt aborts the configure
#     step if it ever disagrees with Cargo.toml, so drift cannot ship.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\sync-version.ps1        # check, report, non-zero on drift
#   powershell -ExecutionPolicy Bypass -File scripts\sync-version.ps1 -Fix   # rewrite the ?v= strings
#   powershell -ExecutionPolicy Bypass -File scripts\sync-version.ps1 -Fix -SkipHtml
#   powershell -ExecutionPolicy Bypass -File scripts\sync-version.ps1 -Tag v0.0.25
# ---------------------------------------------------------------------------
[CmdletBinding()]
param(
    [switch]$Fix,
    [switch]$SkipHtml,
    [string]$Tag = ""
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path -LiteralPath (Split-Path -Parent $PSScriptRoot)).Path

# --- canonical version ------------------------------------------------------
$cargoToml = Join-Path $root "Cargo.toml"
if (-not (Test-Path -LiteralPath $cargoToml)) {
    throw "Cargo.toml not found at $cargoToml — it is the authoritative version source."
}
$match = Select-String -Path $cargoToml -Pattern '^version = "([0-9]+\.[0-9]+\.[0-9]+)"' |
    Select-Object -First 1
if (-not $match) {
    throw "Could not extract ^version = `"X.Y.Z`" from $cargoToml. It is the authoritative release version; refusing to guess."
}
$canonical = $match.Matches[0].Groups[1].Value

$problems = New-Object System.Collections.Generic.List[string]
$changed = New-Object System.Collections.Generic.List[string]

function Get-AssetVersions([string]$text) {
    return @([regex]::Matches($text, '\?v=([0-9A-Za-z._-]+)') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)
}

function Sync-Html([string]$relPath, [string]$label) {
    $path = Join-Path $root $relPath
    if (-not (Test-Path -LiteralPath $path)) {
        # dist/ copies only exist after a `cargo build`; that is not a problem.
        if ($label -eq "dist") {
            Write-Host "  [skip] $relPath (not present yet; build.rs will create it)"
        } else {
            $problems.Add("$relPath is missing")
        }
        return
    }
    $text = Get-Content -LiteralPath $path -Raw
    $found = @(Get-AssetVersions $text)
    if ($found.Count -eq 0) {
        Write-Host "  [skip] $relPath (no ?v= cache-buster)"
        return
    }
    if ($found.Count -eq 1 -and $found[0] -eq $canonical) {
        Write-Host "  [ ok ] $relPath (all cache-busters v=$canonical)"
        return
    }
    if (-not $Fix) {
        $versions = $found -join ", "
        $problems.Add("$relPath has cache-buster version(s) $versions but the release version is $canonical")
        Write-Host "  [DRIFT] $relPath versions=$versions (expected only $canonical)"
        return
    }
    $updated = [regex]::Replace($text, '\?v=[0-9A-Za-z._-]+', "?v=$canonical")
    Set-Content -LiteralPath $path -Value $updated -NoNewline -Encoding UTF8
    $changed.Add($relPath)
    Write-Host "  [fix ] $relPath cache-busters -> v=$canonical"
}

function Check-VersionHeader() {
    $path = Join-Path $root "plugin/version.h"
    if (-not (Test-Path -LiteralPath $path)) {
        $problems.Add("plugin/version.h is missing (must define SLT_VERSION)")
        return
    }
    $m = [regex]::Match((Get-Content -LiteralPath $path -Raw),
        '#define\s+SLT_VERSION\s+"([0-9]+\.[0-9]+\.[0-9]+)"')
    if (-not $m.Success) {
        $problems.Add('plugin/version.h does not define: #define SLT_VERSION "X.Y.Z"')
        return
    }
    $headerVersion = $m.Groups[1].Value
    if ($headerVersion -ne $canonical) {
        $problems.Add("plugin/version.h says $headerVersion but Cargo.toml says $canonical")
        Write-Host "  [DRIFT] plugin/version.h $headerVersion (expected $canonical) — edit the literal by hand"
        return
    }
    Write-Host "  [ ok ] plugin/version.h $headerVersion"
}

Write-Host "Canonical version (Cargo.toml): $canonical"
Write-Host "Checking derived versions:"
if ($Tag) {
    if (-not $Tag.StartsWith("v") -or $Tag.Substring(1) -ne $canonical) {
        $problems.Add("release tag $Tag does not match v$canonical")
        Write-Host "  [DRIFT] release tag $Tag (expected v$canonical)"
    } else {
        Write-Host "  [ ok ] release tag $Tag"
    }
}
Check-VersionHeader

if (-not $SkipHtml) {
    Sync-Html "admin/index.html"        "source"
    Sync-Html "overlay/index.html"      "source"
    Sync-Html "dist/admin/index.html"   "dist"
    Sync-Html "dist/overlay/index.html" "dist"
}

if ($problems.Count -gt 0) {
    Write-Host ""
    Write-Host "Version drift remains:" -ForegroundColor Red
    foreach ($p in $problems) { Write-Host "  - $p" -ForegroundColor Red }
    if (-not $Fix) {
        Write-Host ""
        Write-Host "Re-run with -Fix to rewrite the ?v= cache-busting strings."
    }
    exit 1
}

if ($changed.Count -gt 0) {
    Write-Host ""
    Write-Host "Rewrote $($changed.Count) file(s): $($changed -join ', ')"
    Write-Host "The engine embeds dist/, so rebuild (cargo build) after this."
}
Write-Host "All derived versions agree with Cargo.toml ($canonical)."
exit 0
