#!/usr/bin/env bash
# Drives a simee device with AutoMobile through a private daemon (scripts/automobile-daemon.sh):
# its own one, or the shared one AM_DAEMON_ENV names (scripts/automobile-farm.sh). Prints a
# pass/fail line and the wall time per tool and writes the matrix to <logs>/coverage.tsv.
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

if [[ "$target" == @* ]]; then
  # Let AutoMobile launch the AVD itself; the SDK overlay makes its "emulator" simee.
  shim="${SIMEE_SDK:-$HOME/.simee/sdk}"
  [ -x "$shim/emulator/emulator" ] || "$repo/scripts/sdk-shim.sh" "$shim"
  export ANDROID_HOME="$shim" ANDROID_SDK_ROOT="$shim"
  acquire=(getAndroid --avd-name "${target#@}")
else
  acquire=(getAndroid --device-id "$target")
fi

log=$(mktemp -d "${TMPDIR:-/tmp}/simee-am-run.XXXXXX")
. "$repo/scripts/automobile-daemon.sh"
session=
cleanup() {
  am_release "$session"
  am_daemon_down
  echo "logs: $log (coverage: $log/coverage.tsv)"
}
trap cleanup EXIT
am_daemon_up "$am_dir"

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
  # Something to record: a stop right after the start can come before the encoder's first
  # frame, which under software mode's TCG takes over half a second, and leaves an empty file.
  "swipeOn --direction up"
  "videoRecording --action stop"
  "captureAudio --seconds 1"
  "injectAudioInput --tone {\"frequencyHz\":440,\"durationMs\":500}"
  "deviceSnapshot --action capture --snapshotName simee-harness"
)
pass=0; fail=0
# Wall time per tool call, in ms: bash 5's EPOCHREALTIME, else perl (a few ms of its own startup).
now_ms() {
  if [ -n "${EPOCHREALTIME:-}" ]; then local t=${EPOCHREALTIME/[.,]/}; echo $((t / 1000))
  else perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; fi
}
printf 'tool\tresult\tms\n' > "$log/coverage.tsv"
for line in "${tools[@]}"; do
  # HARNESS_ONLY=<regex> runs just the matching tools.
  if [ -n "${HARNESS_ONLY:-}" ] && ! [[ "$line" =~ $HARNESS_ONLY ]]; then continue; fi
  read -r -a argv <<< "$line"
  t0=$(now_ms)
  if out=$(am --session-uuid "$session" "${argv[@]}" 2>&1); then
    result=pass; pass=$((pass + 1))
  else
    result=fail; fail=$((fail + 1))
    echo "$out" | tail -8 > "$log/fail-$(printf '%s' "$line" | tr -c 'A-Za-z0-9' _).log"
  fi
  ms=$(($(now_ms) - t0))
  printf '%-40s %s %6d ms\n' "$line" "$result" "$ms"
  printf '%s\t%s\t%s\n' "$line" "$result" "$ms" >> "$log/coverage.tsv"
done
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
