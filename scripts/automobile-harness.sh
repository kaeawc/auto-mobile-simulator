#!/usr/bin/env bash
# Drives a simee-booted device with auto-mobile, through a private daemon (its own socket, PID file
# and state directories) so no other auto-mobile daemon on the machine is touched.
#
# Usage: scripts/automobile-harness.sh <udid> [auto-mobile checkout]
# The checkout defaults to ../auto-mobile. It runs from source (bun src/index.ts), so a stale dist/
# build never matters.
set -euo pipefail
udid=${1:?usage: scripts/automobile-harness.sh <udid> [auto-mobile checkout]}
am_dir=$(cd "${2:-$(dirname "$0")/../../auto-mobile}" && pwd)
entry="$am_dir/src/index.ts"
[ -f "$entry" ] || { echo "no $entry: not an auto-mobile checkout" >&2; exit 2; }

root=$(mktemp -d "${TMPDIR:-/tmp}/simee-am.XXXXXX")
sock=$(mktemp -d /tmp/srs-am.XXXX)
export AUTOMOBILE_DATA_DIR="$root/data" AUTOMOBILE_LOG_DIR="$root/logs" AUTOMOBILE_DB_DIR="$root/db" \
  AUTOMOBILE_COORDINATION_DIR="$root/coordination" AUTOMOBILE_ADB_SERVER_COORDINATION_DIR="$root/adb" \
  AUTOMOBILE_AUX_SOCKET_DIR="$sock" AUTOMOBILE_DAEMON_SOCKET_PATH="$sock/d.sock" \
  AUTOMOBILE_DAEMON_PID_FILE_PATH="$root/daemon.pid" AUTOMOBILE_DAEMON_LOCK_FILE_PATH="$root/daemon.lock" \
  AUTOMOBILE_DAEMON_TOKEN_FILE_PATH="$root/daemon.token" AUTOMOBILE_DAEMON_LAUNCH_LOG_PATH="$root/launch.log" \
  AUTOMOBILE_PRIVATE_DAEMON_ORPHAN_IDLE_MS=0 AUTOMOBILE_APPEARANCE_SYNC=off AUTOMOBILE_PROJECT_ROOT="$am_dir"
mkdir -p "$AUTOMOBILE_DATA_DIR" "$AUTOMOBILE_LOG_DIR" "$AUTOMOBILE_DB_DIR" "$AUTOMOBILE_COORDINATION_DIR" "$AUTOMOBILE_ADB_SERVER_COORDINATION_DIR"

am() { (cd "$root" && bun "$entry" --cli "$@"); }
cleanup() {
  # Stop the private daemon through its own socket, PID and lock paths (still exported) before
  # its socket directory goes: removing the directory doesn't stop it, and an orphan keeps one of
  # AutoMobile's 3000-3010 ports. The PID file is JSON; its pid is the fallback if stop fails.
  if [ -f "$AUTOMOBILE_DAEMON_PID_FILE_PATH" ]; then
    (cd "$root" && bun "$entry" --daemon stop) </dev/null >>"$root/daemon-stop.log" 2>&1 || true
    pid=$(jq -r '.pid // empty' "$AUTOMOBILE_DAEMON_PID_FILE_PATH" 2>/dev/null || true)
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      echo "daemon $pid didn't stop on request; sent SIGTERM" >&2
    fi
  fi
  rm -rf "$sock"
  echo "logs: $root"
}
trap cleanup EXIT

echo "== getApple"
out=$(am getApple --udid "$udid")
echo "$out" | tail -20
session=$(echo "$out" | grep -oE 'sessionUuid[^0-9a-f]*[0-9a-f-]{36}' | head -1 | grep -oE '[0-9a-f-]{36}' || true)
[ -n "$session" ] || { echo "no session minted" >&2; exit 1; }
echo "== observe (session $session)"
am --session-uuid "$session" observe | tail -40
echo "== pressButton home"
am --session-uuid "$session" pressButton --button home | tail -5
