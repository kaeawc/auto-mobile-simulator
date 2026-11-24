#!/usr/bin/env bash
# Drives a simee device with AutoMobile through a private daemon (its own socket, PID file and
# state directories), so no other AutoMobile daemon on the machine is touched. Prints a
# pass/fail line per tool and writes the matrix to <logs>/coverage.tsv.
#
#   scripts/automobile-harness-android.sh emulator-5554 [auto-mobile checkout]   # attach to a running simee
#   scripts/automobile-harness-android.sh @<avd> [auto-mobile checkout]          # AutoMobile boots it (via `make shim`)
#
# The checkout defaults to ../auto-mobile and runs from source (bun src/index.ts).
# HARNESS_ONLY=<regex> limits the run to matching tool lines.
set -euo pipefail
target=${1:?usage: scripts/automobile-harness-android.sh <emulator-PORT|@avd> [auto-mobile checkout]}
repo=$(cd "$(dirname "$0")/.." && pwd)
am_dir=$(cd "${2:-$repo/../auto-mobile}" && pwd)
entry="$am_dir/src/index.ts"
[ -f "$entry" ] || { echo "no $entry: not an auto-mobile checkout" >&2; exit 2; }

root=$(mktemp -d "${TMPDIR:-/tmp}/simee-am.XXXXXX")
sock=$(mktemp -d /tmp/ers-am.XXXX)
export AUTOMOBILE_DATA_DIR="$root/data" AUTOMOBILE_LOG_DIR="$root/logs" AUTOMOBILE_DB_DIR="$root/db" \
  AUTOMOBILE_COORDINATION_DIR="$root/coordination" AUTOMOBILE_ADB_SERVER_COORDINATION_DIR="$root/adb" \
  AUTOMOBILE_AUX_SOCKET_DIR="$sock" AUTOMOBILE_DAEMON_SOCKET_PATH="$sock/d.sock" \
  AUTOMOBILE_DAEMON_PID_FILE_PATH="$root/daemon.pid" AUTOMOBILE_DAEMON_LOCK_FILE_PATH="$root/daemon.lock" \
  AUTOMOBILE_DAEMON_TOKEN_FILE_PATH="$root/daemon.token" AUTOMOBILE_DAEMON_LAUNCH_LOG_PATH="$root/launch.log" \
  AUTOMOBILE_PRIVATE_DAEMON_ORPHAN_IDLE_MS=0 AUTOMOBILE_APPEARANCE_SYNC=off AUTOMOBILE_PROJECT_ROOT="$am_dir"
mkdir -p "$AUTOMOBILE_DATA_DIR" "$AUTOMOBILE_LOG_DIR" "$AUTOMOBILE_DB_DIR" "$AUTOMOBILE_COORDINATION_DIR" "$AUTOMOBILE_ADB_SERVER_COORDINATION_DIR"

if [[ "$target" == @* ]]; then
  # Let AutoMobile launch the AVD itself; the SDK overlay makes its "emulator" simee.
  shim="${SIMEE_SDK:-$HOME/.simee/sdk}"
  [ -x "$shim/emulator/emulator" ] || "$repo/scripts/sdk-shim.sh" "$shim"
  export ANDROID_HOME="$shim" ANDROID_SDK_ROOT="$shim"
  acquire=(getAndroid --avd-name "${target#@}")
else
  acquire=(getAndroid --device-id "$target")
fi

am() { (cd "$root" && bun "$entry" --cli "$@"); }
cleanup() {
  if [ -f "$AUTOMOBILE_DAEMON_PID_FILE_PATH" ]; then
    pid=$(tr -dc 0-9 < "$AUTOMOBILE_DAEMON_PID_FILE_PATH")
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  fi
  rm -rf "$sock"
  echo "logs: $root (coverage: $root/coverage.tsv)"
}
trap cleanup EXIT

echo "== ${acquire[*]}"
out=$(am "${acquire[@]}" 2>&1) || { echo "$out" | tail -30; echo "acquire failed" >&2; exit 1; }
echo "$out" | tail -15
session=$(echo "$out" | grep -oE 'sessionUuid[^0-9a-f]*[0-9a-f-]{36}' | head -1 | grep -oE '[0-9a-f-]{36}' || true)
[ -n "$session" ] || { echo "no session minted" >&2; exit 1; }

# One line per tool: name, then arguments. Extend as simee grows; the matrix shows what's left.
tools=(
  "observe"
  "listDevices"
  "pressButton --button home"
  "swipeOn --direction up"
  "pressButton --button home"
  "tapAt --x 540 --y 1200"
  "launchApp --appId com.android.settings"
  "swipeOn --direction up"
  "rotate --orientation landscape --lockOrientation true"
  "rotate --orientation portrait"
  "pressButton --button back"
  "recentApps"
  "homeScreen"
  "listApps"
  "getDeviceState"
  "setSensor --sensor light --value [123]"
  "setSensor --sensor acceleration --value [0,9.8,0]"
  "setSensor --sensor light --reset true"
  "shake --duration 500"
  "clipboard --action copy --text simee"
  "biometricAuth --action match --modality fingerprint"
  "sendSms --phoneNumber 5551234 --message hello"
  "phoneCall --action call --phoneNumber 5551234"
  "phoneCall --action cancel --phoneNumber 5551234"
  "statusBar --action get"
  "wakeAndUnlock"
  "setDeviceState --location {\"mode\":\"static\",\"latitude\":37.422,\"longitude\":-122.084}"
  "setDeviceState --networkCondition {\"profile\":\"3g\"}"
  "setDeviceState --networkCondition {\"reset\":true}"
  "displayConfig --theme dark"
  "displayConfig --theme light"
  "getLogs --since sessionStart --minPriority E"
  "systemTray --action open"
  "systemTray --action close"
  "openLink --url https://example.com"
  "pressButton --button home"
  "multiTouch --action tap --x 540 --y 1200"
  "pinchOn --direction in"
  "videoRecording --action start"
  "videoRecording --action stop"
  "captureAudio --seconds 1"
  "injectAudioInput --tone {\"frequencyHz\":440,\"durationMs\":500}"
  "deviceSnapshot --action capture --snapshotName simee-harness"
)
pass=0; fail=0
printf 'tool\tresult\n' > "$root/coverage.tsv"
for line in "${tools[@]}"; do
  # HARNESS_ONLY=<regex> runs just the matching tools.
  if [ -n "${HARNESS_ONLY:-}" ] && ! [[ "$line" =~ $HARNESS_ONLY ]]; then continue; fi
  read -r -a argv <<< "$line"
  if out=$(am --session-uuid "$session" "${argv[@]}" 2>&1); then
    result=pass; pass=$((pass + 1))
  else
    result=fail; fail=$((fail + 1))
    echo "$out" | tail -8 > "$root/fail-$(printf '%s' "$line" | tr -c 'A-Za-z0-9' _).log"
  fi
  printf '%-40s %s\n' "$line" "$result"
  printf '%s\t%s\n' "$line" "$result" >> "$root/coverage.tsv"
done
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
