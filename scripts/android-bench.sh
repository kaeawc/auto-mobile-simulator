#!/usr/bin/env bash
# Boot one AVD and measure it, the same way for emu-rs and the stock emulator, so results compare.
#
#   scripts/bench.sh <label> <avd> [emu|stock] [extra args...]   -> one key=value line, also in bench/<label>.result
#
# Metrics: seconds to `adb device` and to sys.boot_completed; median-of-3 latency of common adb
# shell calls; guest CPU microbenchmarks; the emulator process's peak RSS. Runs with a stripped
# environment (no Java on PATH). Stops the instance it started, by PID.
set -uo pipefail
label=${1:?usage: scripts/bench.sh <label> <avd> [emu|stock] [args...]}
avd=${2:?avd}; kind=${3:-emu}; shift $(( $# >= 3 ? 3 : $# ))
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
adb="$sdk/platform-tools/adb"
out="$repo/bench"; mkdir -p "$out"
port=${PORT:-5600}; serial="emulator-$port"
case "$kind" in
  emu) (cd "$repo" && cargo build -q --release -p emu) && codesign -s - -f --entitlements "$repo/scripts/hypervisor.entitlements" "$repo/target/release/emu" 2>/dev/null
       bin="$repo/target/release/emu" ;;
  stock) bin="$sdk/emulator/emulator" ;;
  *) echo "kind must be emu or stock" >&2; exit 2 ;;
esac
now() { python3 -c 'import time;print(time.time())'; }
t0=$(now)
env -i HOME="$HOME" USER="$USER" TMPDIR="$TMPDIR" PATH="/usr/bin:/bin:/usr/sbin:/sbin" ANDROID_HOME="$sdk" ANDROID_SDK_ROOT="$sdk" \
  "$bin" -avd "$avd" -port "$port" -no-window -no-audio -no-boot-anim -no-snapshot-save "$@" > "$out/$label.log" 2>&1 &
pid=$!
deadline=$(( $(date +%s) + ${BOOT_TIMEOUT:-600} )); state=timeout; t_adb=""; rss=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  kill -0 $pid 2>/dev/null || { state=exited; break; }
  # The stock launcher execs a qemu child; count the whole process tree.
  r=$(ps -o rss= -p $pid $(pgrep -P $pid) 2>/dev/null | awk '{s+=$1} END{print s+0}'); [ "$r" -gt "$rss" ] && rss=$r
  [ -z "$t_adb" ] && "$adb" -s "$serial" get-state 2>/dev/null | grep -q device && t_adb=$(python3 -c "print(round($(now)-$t0,1))")
  [ "$("$adb" -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && { state=booted; break; }
  sleep 1
done
t_boot=$(python3 -c "print(round($(now)-$t0,1))")
lat() { local s; s=$(now); "$adb" -s "$serial" shell "$@" >/dev/null 2>&1; python3 -c "print(round($(now)-$s,2))"; }
med3() { printf '%s\n%s\n%s\n' "$(lat "$@")" "$(lat "$@")" "$(lat "$@")" | sort -n | sed -n 2p; }
m=""
if [ -n "$t_adb" ]; then
  m="echo=$(med3 echo hi) getprop=$(med3 getprop ro.build.version.sdk)"
  m+=" loop100k=$(lat 'i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done')"
  m+=" md5x4=$(lat 'for j in 1 2 3 4; do (dd if=/dev/zero bs=1048576 count=64 2>/dev/null | md5sum >/dev/null) & done; wait')"
  [ "$state" = booted ] && m+=" pm_list=$(med3 pm list packages) am_start_W=$(med3 am start -W -n com.android.settings/.Settings)"
fi
line="label=$label kind=$kind state=$state adb_s=${t_adb:-} boot_s=$t_boot peak_rss_mb=$((rss / 1024)) $m"
echo "$line" | tee "$out/$label.result"
"$adb" -s "$serial" emu kill >/dev/null 2>&1; sleep 3; kill $pid 2>/dev/null; wait $pid 2>/dev/null
exit 0
