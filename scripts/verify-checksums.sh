#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# verify-checksums.sh — verify every `<file>.sha256` sidecar in a directory.
#
# The packaging scripts (scripts/package-plugin.ps1, scripts/package-plugin.sh)
# write `<archive>.sha256` next to each archive. This script re-verifies those
# sidecars after the round trip through GitHub Actions artifact storage.
#
# Usage: bash scripts/verify-checksums.sh <dir> [--allow-empty]
#
#   <dir>           directory to search recursively for *.sha256
#   --allow-empty   succeed when no sidecar is found (used where the producing
#                   job genuinely does not emit any); without it, finding no
#                   sidecar is an error, so "verified nothing" can never pass
#                   for "verified everything".
# ---------------------------------------------------------------------------
set -euo pipefail

DIR="${1:-}"
ALLOW_EMPTY=0
for arg in "${@:2}"; do
    case "$arg" in
        --allow-empty) ALLOW_EMPTY=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

if [ -z "$DIR" ]; then
    echo "usage: bash scripts/verify-checksums.sh <dir> [--allow-empty]" >&2
    exit 2
fi
if [ ! -d "$DIR" ]; then
    echo "::error::checksum directory '$DIR' does not exist" >&2
    exit 1
fi

verified=0
unverified=0

while IFS= read -r sumfile; do
    [ -n "$sumfile" ] || continue
    echo "== verifying $sumfile"
    # sha256sum -c resolves the recorded path relative to the CWD, so run it
    # from the sidecar's own directory (the packaging scripts record a bare
    # basename there).
    ( cd "$(dirname "$sumfile")" && sha256sum -c "$(basename "$sumfile")" )
    verified=$((verified + 1))
done < <(find "$DIR" -type f -name '*.sha256' | sort)

while IFS= read -r artifact; do
    [ -n "$artifact" ] || continue
    case "$artifact" in
        *.sha256) continue ;;
    esac
    if [ ! -f "$artifact.sha256" ]; then
        echo "::warning::$artifact has no .sha256 sidecar and was NOT verified"
        unverified=$((unverified + 1))
    fi
done < <(find "$DIR" -type f | sort)

echo "verified=$verified unverified(no sidecar)=$unverified"

if [ "$verified" -eq 0 ]; then
    if [ "$ALLOW_EMPTY" -eq 1 ]; then
        echo "::notice::no .sha256 sidecar found under '$DIR' (allowed for this job)"
        exit 0
    fi
    echo "::error::no .sha256 sidecar found under '$DIR'; refusing to publish unverified artifacts" >&2
    exit 1
fi
