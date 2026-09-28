#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# ci-secret-guard.sh — release gate against credentials and unedited templates.
#
# Run from the repository root by both release workflows:
#   bash scripts/ci-secret-guard.sh release     # source tree + built packages
#   bash scripts/ci-secret-guard.sh artifacts   # published artifacts too
#
# Every pattern is absolute-value (no fuzzy "looks like a secret" heuristics),
# stops with a non-zero exit and a GitHub `::error::` annotation, and is
# explained below so the list stays reviewable.
# ---------------------------------------------------------------------------
set -euo pipefail

ARTIFACT_DIR="${1:-artifacts}"
fail=0

# --- 1+2. Config files must not carry a credential -------------------------
# The shipped template is `api_key = ""`; every other value is either a real
# secret (rule 1) or a placeholder somebody forgot to fill in (rule 2).
CFG_FILES=""
if [ -d . ]; then
    CFG_FILES="$(find . -maxdepth 2 \( -name 'config.toml' -o -name 'config.json' \) \
        -not -path './target/*' -not -path './build/*' -print 2>/dev/null || true)"
fi

if [ -n "$CFG_FILES" ]; then
    # Rule 1: at least one character between the quotes => a non-empty value.
    if grep -nE '"?(api[_-]?key|access[_-]?key|secret[_-]?key|client[_-]?secret)"?[[:space:]]*[:=][[:space:]]*"[^"]+"' $CFG_FILES; then
        echo "::error::config file contains a non-empty credential value (api_key/access_key/secret_key/client_secret)"
        fail=1
    fi
    # Rule 2: the same keys holding obvious placeholder text.
    if grep -nEi '"?(api[_-]?key|access[_-]?key|secret[_-]?key|client[_-]?secret)"?[[:space:]]*[:=][[:space:]]*"[^"]*(your[-_ ]?key|xxx+|changeme|change[-_ ]?me|placeholder|dummy|example|todo|fixme|sk-\.\.\.)[^"]*"' $CFG_FILES; then
        echo "::error::config file still contains a placeholder credential"
        fail=1
    fi
else
    echo "note: no config.toml/config.json found at depth <= 2; rules 1-2 not exercised"
fi

# --- 3. Private key material ----------------------------------------------
# Covers RSA/EC/OPENSSH/PGP blocks in the tree, in dist/ templates and in the
# workflow files themselves.
if grep -rnE -- '-----BEGIN [A-Z ]*PRIVATE KEY-----' \
     --exclude-dir=.git --exclude-dir=target --exclude-dir=build . ; then
    echo "::error::private key header found in the source tree"
    fail=1
fi

# --- 4. Provider-shaped live tokens ---------------------------------------
# Deliberately strict, so ordinary prose (and the deliberate leak-canary
# fixtures in the Rust tests, longest 24 chars) cannot trip the gate:
#   (^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{32,64}  OpenAI / DashScope secret key
#   sk_live_[A-Za-z0-9]{16,}                   Stripe-style live secret
#   \bAKIA[0-9A-Z]{16}\b                       AWS access key id
#   \bgh[pous]_[A-Za-z0-9]{36}\b               GitHub token (exactly 36 chars)
#   \bAIza[0-9A-Za-z_-]{35}\b                  Google API key (exactly 35 chars)
# The leading boundary stops a match inside an identifier such as the Rust
# const `MODEL_KEY`.
TOKEN_PATTERN='(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{32,64}|sk_live_[A-Za-z0-9]{16,}|\bAKIA[0-9A-Z]{16}\b|\bgh[pous]_[A-Za-z0-9]{36}\b|\bAIza[0-9A-Za-z_-]{35}\b'
if grep -rnE -- "$TOKEN_PATTERN" \
     --exclude-dir=.git --exclude-dir=target --exclude-dir=build . ; then
    echo "::error::live-looking credential token found in the source tree"
    fail=1
fi

# --- 5. The same token shapes inside what is about to be published --------
# `-a` treats the compressed bytes of .zip/.tar.gz as text, which is enough to
# catch a default config that was accidentally filled in before packaging.
if [ -e "$ARTIFACT_DIR" ]; then
    if grep -ralE -- "$TOKEN_PATTERN|-----BEGIN [A-Z ]*PRIVATE KEY-----" "$ARTIFACT_DIR" ; then
        echo "::error::credential material found in '$ARTIFACT_DIR'"
        fail=1
    fi
else
    echo "note: artifact directory '$ARTIFACT_DIR' not present; artifact scan skipped"
fi

if [ "$fail" -ne 0 ]; then
    echo "::error::secret guard failed; refusing to publish"
    exit 1
fi
echo "secret guard passed ($ARTIFACT_DIR)"
