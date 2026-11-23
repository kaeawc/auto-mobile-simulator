#!/usr/bin/env bash
# Points git at the repository's hooks (.githooks/): pre-push runs scripts/check.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
echo "hooks enabled: pre-push runs scripts/check.sh (skip once with git push --no-verify)"
