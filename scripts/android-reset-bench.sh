#!/usr/bin/env bash
# What a guest reset costs on emu: boot an AVD, reset the guest RUNS times, and time each reset to
# the next boot's first host frame and to sys.boot_completed; or time a stop (`kill -INT`) to the
# process's exit. For A/B runs, point EMU at each build (the script doesn't build).
#
#   scripts/android-reset-bench.sh <avd> hard|reboot|exit [runs]   -> one key=value line per run, then medians
#
# hard    `echo b > /proc/sysrq-trigger` under `adb root`: the kernel resets at once, nothing
#         closes on the way down (a panic or a watchdog looks the same to the device)
# reboot  `adb reboot`: an orderly reboot
# exit    boots, then `kill -INT` the emu PID; seconds to exit and the exit status. One boot per
#         run, so each stop is of a freshly booted device.
#
# The reset is when emu's log prints `emu: stopped after ...: Reset`. The first host frame is the
# first `adb emu screenshot` after it (polled every POLL s, default 0.1) that is neither the frame
# taken just before the reset nor all black: the next boot's first content, the same on a build
# that shows the last boot's final frame until then and one that shows black. resume_s is when
# emu printed `emu: host frames resume` (a Vulkan-only renderer's post notices restarting; empty
# on builds or renderers without it). A reset whose boot doesn't complete in RESET_TIMEOUT s
# (default 120) reports state=timeout. Stops the emu it started by PID, never by name.
#
# Env: EMU (default target/release/emu, signed with scripts/hypervisor.entitlements), PORT
# (default 5600), BOOT_TIMEOUT (default 300).
set -uo pipefail
avd=${1:?usage: scripts/android-reset-bench.sh <avd> hard|reboot|exit [runs]}
mode=${2:?mode: hard, reboot or exit}; runs=${3:-3}
case "$mode" in hard | reboot | exit) ;; *) echo "mode must be hard, reboot or exit" >&2; exit 2 ;; esac
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
emu=${EMU:-$repo/target/release/emu}
port=${PORT:-5600}; serial="emulator-$port"
adb() { "$sdk/platform-tools/adb" -s "$serial" "$@"; }
work=$(mktemp -d "${TMPDIR:-/tmp}/reset-bench.XXXXXX")
log="$work/emu.log"
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { awk -v a="$(now)" -v b="$1" 'BEGIN { printf "%.2f\n", a - b }'; }
pid=""
stop() { [ -n "$pid" ] && kill -INT "$pid" 2>/dev/null && wait "$pid" 2>/dev/null; pid=""; }
trap 'stop; rm -rf "$work"' EXIT

start() {
  : > "$log"
  "$emu" -avd "$avd" -port "$port" -no-window -no-audio -no-boot-anim -no-snapshot-save -verbose > "$log" 2>&1 &
  pid=$!
  echo "emu pid=$pid" >&2
}

# Waits until sys.boot_completed is 1, up to $1 s; 0 when it is.
booted() {
  local deadline=$((SECONDS + $1))
  while [ "$SECONDS" -lt "$deadline" ]; do
    kill -0 "$pid" 2>/dev/null || return 1
    [ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && return 0
    sleep 0.15
  done
  return 1
}

# Whether PNG $1 is all black (colour channels zero; alpha ignored).
black() {
  sips -s format bmp "$1" --out "$work/shot.bmp" >/dev/null 2>&1 || return 1
  python3 -I -c '
import sys
d = open(sys.argv[1], "rb").read()
px, bpp = d[int.from_bytes(d[10:14], "little"):], int.from_bytes(d[28:30], "little")
rgb = px[0::4] + px[1::4] + px[2::4] if bpp == 32 else px
sys.exit(0 if rgb.count(0) == len(rgb) else 1)' "$work/shot.bmp"
}

shot() { adb emu screenshot "$1" 2>/dev/null | grep -q '^OK' && [ -s "$1" ]; }

median() { sort -n | awk '{ v[NR] = $1 } END { if (NR) print v[int((NR + 1) / 2)] }'; }

results="$work/results"
: > "$results"
if [ "$mode" = exit ]; then
  for run in $(seq 1 "$runs"); do
    start
    if ! booted "${BOOT_TIMEOUT:-300}"; then echo "run=$run state=boot-timeout"; stop; continue; fi
    sleep 5 # settled
    t=$(now); kill -INT "$pid"; wait "$pid"; status=$?; exit_s=$(since "$t"); pid=""
    echo "mode=exit run=$run exit_s=$exit_s status=$status" | tee -a "$results"
  done
  echo "mode=exit runs=$runs median_exit_s=$(sed -n 's/.*exit_s=\([0-9.]*\).*/\1/p' "$results" | median)"
  exit 0
fi

start
booted "${BOOT_TIMEOUT:-300}" || { echo "state=boot-timeout (log: tail of $log)"; tail -5 "$log"; exit 1; }
for run in $(seq 1 "$runs"); do
  sleep 5 # settled
  if [ "$mode" = hard ]; then
    adb root >/dev/null 2>&1; adb wait-for-device; sleep 1
  fi
  shot "$work/before.png" || { echo "run=$run state=no-screenshot"; break; }
  resets=$(grep -c ': Reset$' "$log")
  if [ "$mode" = hard ]; then
    adb shell 'echo b > /proc/sysrq-trigger' >/dev/null 2>&1 &
    trig=$!
  else
    adb reboot >/dev/null 2>&1 &
    trig=$!
  fi
  deadline=$((SECONDS + 30))
  until [ "$(grep -c ': Reset$' "$log")" -gt "$resets" ]; do
    [ "$SECONDS" -lt "$deadline" ] || { echo "run=$run state=no-reset"; break 2; }
    sleep 0.02
  done
  t=$(now)
  resumes=$(grep -c '^emu: host frames resume' "$log")
  frame_s="" resume_s="" state=timeout deadline=$((SECONDS + ${RESET_TIMEOUT:-120}))
  while [ "$SECONDS" -lt "$deadline" ] && [ -z "$frame_s" ]; do
    [ -z "$resume_s" ] && [ "$(grep -c '^emu: host frames resume' "$log")" -gt "$resumes" ] && resume_s=$(since "$t")
    if shot "$work/now.png" && ! cmp -s "$work/now.png" "$work/before.png" && ! black "$work/now.png"; then
      frame_s=$(since "$t")
    fi
    sleep "${POLL:-0.1}"
  done
  boot_s=""
  if [ -n "$frame_s" ]; then
    left=$((deadline - SECONDS)); [ "$left" -lt 1 ] && left=1
    booted "$left" && { boot_s=$(since "$t"); state=booted; }
  fi
  [ -z "$resume_s" ] && [ "$(grep -c '^emu: host frames resume' "$log")" -gt "$resumes" ] && resume_s="<$(since "$t")"
  kill "$trig" 2>/dev/null; wait "$trig" 2>/dev/null
  echo "mode=$mode run=$run state=$state first_frame_s=$frame_s resume_s=$resume_s boot_completed_s=$boot_s" | tee -a "$results"
  [ "$state" = booted ] || break
done
med() { sed -n "s/.* $1=\([0-9.][0-9.]*\).*/\1/p" "$results" | median; }
echo "mode=$mode runs=$(grep -c state=booted "$results")/$runs median_first_frame_s=$(med first_frame_s) median_boot_completed_s=$(med boot_completed_s)"
t=$(now); kill -INT "$pid"; wait "$pid"; status=$?; echo "exit_s=$(since "$t") status=$status"; pid=""
