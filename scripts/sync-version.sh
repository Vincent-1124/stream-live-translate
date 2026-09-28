#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# sync-version.sh — macOS / Linux counterpart of scripts/sync-version.ps1.
#
# The single authoritative version of this repository is the `version` field
# of the root Cargo.toml. This script makes the derived cache-busting `?v=`
# query strings in admin/index.html and overlay/index.html agree with it, and
# *reports* (never rewrites) plugin/version.h, which plugin/CMakeLists.txt
# already verifies at configure time.
#
# Usage:
#   bash scripts/sync-version.sh          # check, report drift, non-zero exit
#   bash scripts/sync-version.sh --fix    # rewrite the ?v= strings
#   bash scripts/sync-version.sh --fix --skip-html
#   bash scripts/sync-version.sh --tag v0.0.25
# ---------------------------------------------------------------------------
set -euo pipefail

FIX=0
SKIP_HTML=0
RELEASE_TAG=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --fix)       FIX=1 ;;
        --skip-html) SKIP_HTML=1 ;;
        --tag)
            [ "$#" -ge 2 ] || { echo "--tag requires a value" >&2; exit 2; }
            RELEASE_TAG="$2"
            shift
            ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# --- canonical version ------------------------------------------------------
CANONICAL="$(sed -n 's/^version = "\(.*\)".*/\1/p' "$ROOT/Cargo.toml" | head -n1)"
if [ -z "$CANONICAL" ]; then
    echo "FATAL: could not extract ^version = \"X.Y.Z\" from $ROOT/Cargo.toml" >&2
    echo "       Cargo.toml is the authoritative release version; refusing to guess." >&2
    exit 1
fi

PROBLEMS=0
echo "Canonical version (Cargo.toml): $CANONICAL"
echo "Checking derived versions:"

if [ -n "$RELEASE_TAG" ]; then
    TAG_VERSION="${RELEASE_TAG#v}"
    if [ "$RELEASE_TAG" = "$TAG_VERSION" ] || [ "$TAG_VERSION" != "$CANONICAL" ]; then
        echo "  [DRIFT] release tag $RELEASE_TAG (expected v$CANONICAL)"
        PROBLEMS=1
    else
        echo "  [ ok ] release tag $RELEASE_TAG"
    fi
fi

# --- plugin/version.h (report only) -----------------------------------------
HEADER="$ROOT/plugin/version.h"
if [ ! -f "$HEADER" ]; then
    echo "  [DRIFT] plugin/version.h is missing (must define SLT_VERSION)"
    PROBLEMS=1
else
    HEADER_VERSION="$(sed -n 's/^#define[[:space:]]\{1,\}SLT_VERSION[[:space:]]\{1,\}"\(.*\)".*/\1/p' "$HEADER" | head -n1)"
    if [ -z "$HEADER_VERSION" ]; then
        echo '  [DRIFT] plugin/version.h has no: #define SLT_VERSION "X.Y.Z"'
        PROBLEMS=1
    elif [ "$HEADER_VERSION" != "$CANONICAL" ]; then
        echo "  [DRIFT] plugin/version.h $HEADER_VERSION (expected $CANONICAL) - edit the literal by hand"
        PROBLEMS=1
    else
        echo "  [ ok ] plugin/version.h $HEADER_VERSION"
    fi
fi

# --- cache-busting query strings -------------------------------------------
sync_html() {
    local rel="$1" label="$2"
    local path="$ROOT/$rel"
    if [ ! -f "$path" ]; then
        if [ "$label" = "dist" ]; then
            echo "  [skip] $rel (not present yet; build.rs creates it)"
        else
            echo "  [DRIFT] $rel is missing"
            PROBLEMS=1
        fi
        return
    fi
    local found versions
    versions="$(grep -oE '\?v=[0-9A-Za-z._-]+' "$path" | sed 's/^?v=//' | sort -u || true)"
    if [ -z "$versions" ]; then
        echo "  [skip] $rel (no ?v= cache-buster)"
        return
    fi
    if [ "$versions" = "$CANONICAL" ]; then
        echo "  [ ok ] $rel (all cache-busters v=$CANONICAL)"
        return
    fi
    if [ "$FIX" -eq 0 ]; then
        echo "  [DRIFT] $rel has cache-buster version(s): $(printf '%s' "$versions" | tr '\n' ' ') (expected only $CANONICAL)"
        PROBLEMS=1
        return
    fi
    # In-place, same-file rewrite (no temp file) so permissions are preserved.
    sed -i.bak "s/?v=[0-9A-Za-z._-]\{1,\}/?v=$CANONICAL/g" "$path"
    rm -f "$path.bak"
    echo "  [fix ] $rel cache-busters -> v=$CANONICAL"
}

if [ "$SKIP_HTML" -eq 0 ]; then
    sync_html "admin/index.html"        source
    sync_html "overlay/index.html"      source
    sync_html "dist/admin/index.html"   dist
    sync_html "dist/overlay/index.html" dist
fi

if [ "$PROBLEMS" -ne 0 ]; then
    echo ""
    echo "Version drift remains (see [DRIFT] lines above)." >&2
    if [ "$FIX" -eq 0 ]; then
        echo "Re-run with --fix to rewrite the ?v= cache-busting strings." >&2
    fi
    exit 1
fi

echo "All derived versions agree with Cargo.toml ($CANONICAL)."
