#!/usr/bin/env bash
# Runs the AutoMobile harness on many devices at once through ONE private AutoMobile daemon: one
# session per device, all concurrent, the daemon stopped once at the end.
#
#   scripts/automobile-farm.sh <target>... [-- <auto-mobile checkout>]
#
# A target is an iOS UDID (scripts/automobile-harness.sh), or emulator-<port> / @<avd>
# (scripts/automobile-harness-android.sh). Each device's output goes to <logs>/<target>.log; the
# script prints one line per device and exits non-zero if any failed. HARNESS_ONLY applies to
# every Android run.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
targets=()
while [ $# -gt 0 ]; do
  [ "$1" = -- ] && {
    shift
    break
  }
  targets+=("$1")
  shift
done
[ ${#targets[@]} -gt 0 ] || {
  echo "usage: scripts/automobile-farm.sh <target>... [-- <auto-mobile checkout>]" >&2
  exit 2
}
am_dir=$(cd "${1:-$repo/../auto-mobile}" && pwd)

# AVDs that AutoMobile boots itself must find simee as "emulator": the daemon keeps the
# environment it starts with, so the SDK overlay is set before it starts.
for t in "${targets[@]}"; do
  if [[ "$t" == @* ]]; then
    shim="${SIMEE_SDK:-$HOME/.simee/sdk}"
    [ -x "$shim/emulator/emulator" ] || "$repo/scripts/sdk-shim.sh" "$shim"
    export ANDROID_HOME="$shim" ANDROID_SDK_ROOT="$shim"
    break
  fi
done

# shellcheck source=automobile-daemon.sh
. "$repo/scripts/automobile-daemon.sh"
log=$(mktemp -d "${TMPDIR:-/tmp}/simee-am-farm.XXXXXX")
pids=()
cleanup() {
  # A harness started here exits on its own once it finishes; on an interrupt, stop exactly those.
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  am_daemon_down
  echo "logs: $log"
}
trap cleanup EXIT
am_daemon_up "$am_dir"
export AM_DAEMON_ENV="$am_root/env.sh"

for t in "${targets[@]}"; do
  case "$t" in
    emulator-* | @*) script=automobile-harness-android.sh ;;
    *) script=automobile-harness.sh ;;
  esac
  "$repo/scripts/$script" "$t" "$am_dir" >"$log/${t#@}.log" 2>&1 &
  pids+=("$!")
done

failed=0
for i in "${!targets[@]}"; do
  t=${targets[$i]}
  if wait "${pids[$i]}"; then
    result=pass
  else
    result=fail
    failed=$((failed + 1))
  fi
  summary=$(grep -E '^== [0-9]+ passed' "$log/${t#@}.log" | tail -1 || true)
  printf '%-40s %s %s\n' "$t" "$result" "${summary#== }"
done
pids=()
echo "== ${#targets[@]} devices, $failed failed, one daemon (pid $(am_daemon_pid))"
[ "$failed" -eq 0 ]
