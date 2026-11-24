#!/usr/bin/env bash
# Drives a simee-booted device with AutoMobile through a private daemon
# (scripts/automobile-daemon.sh): its own one, or the shared one AM_DAEMON_ENV names
# (scripts/automobile-farm.sh), so no other AutoMobile daemon on the machine is touched.
#
# Usage: scripts/automobile-harness.sh <udid> [auto-mobile checkout]
# The checkout defaults to ../auto-mobile. It runs from source (bun src/index.ts), so a stale dist/
# build never matters.
set -euo pipefail
udid=${1:?usage: scripts/automobile-harness.sh <udid> [auto-mobile checkout]}
repo=$(cd "$(dirname "$0")/.." && pwd)
am_dir=$(cd "${2:-$repo/../auto-mobile}" && pwd)

. "$repo/scripts/automobile-daemon.sh"
session=
cleanup() {
  am_release "$session"
  am_daemon_down
}
trap cleanup EXIT
am_daemon_up "$am_dir"

echo "== getApple"
out=$(am getApple --udid "$udid")
echo "$out" | tail -20
session=$(echo "$out" | grep -oE 'sessionUuid[^0-9a-f]*[0-9a-f-]{36}' | head -1 | grep -oE '[0-9a-f-]{36}' || true)
[ -n "$session" ] || { echo "no session minted" >&2; exit 1; }
echo "== observe (session $session)"
am --session-uuid "$session" observe | tail -40
echo "== pressButton home"
am --session-uuid "$session" pressButton --button home | tail -5
