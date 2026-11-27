#!/usr/bin/env bash
# Android guest resets on emu: a hard reset, `adb reboot`, and a stop, timed from the trigger.
#
#   scripts/bench/android-reset.sh [--modes "hard reboot exit"] [--baseline EMU] [--emu EMU]
#                                  [--image DIR] [--port N] [--rounds N] [--quick] [--out DIR]
#
# Creates an AVD (720x1280, 4 vCPUs, 2 GB), boots it once (its first boot, not counted), then each
# round runs scripts/android-reset-bench.sh <avd> <mode> 1 for every mode and build, the builds
# alternating, and deletes the AVD:
#   hard    `echo b > /proc/sysrq-trigger` under `adb root`: trigger to the next boot's first host
#           frame and to sys.boot_completed
#   reboot  `adb reboot`: the same two times, including the guest's orderly shutdown
#   exit    `kill -INT` of a freshly booted emu: seconds to the process's exit, and its status
# --baseline EMU adds a second build (e.g. the change's parent, built into its own target dir and
# signed with scripts/hypervisor.entitlements), for a before/after; --emu replaces this checkout's
# release build. No stock comparison: the published rows compare emu builds.
#
# Defaults: --modes "hard reboot exit", --image system-images/android-36/google_apis/arm64-v8a,
# --port 5720. --quick: one round of hard only.
# Reproduces docs/benchmarks.md "Emulator internals", the reset rows.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
modes="hard reboot exit"
baseline=""
image=system-images/android-36/google_apis/arm64-v8a
port=5720
modes_set=0
while [ $# -gt 0 ]; do
  case $1 in
    --modes)
      modes=$2
      modes_set=1
      shift 2
      ;;
    --baseline)
      baseline=$2
      shift 2
      ;;
    --emu)
      BENCH_EMU=$2
      shift 2
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && [ "$modes_set" = 0 ] && modes=hard
[ "$(uname -s)" = Darwin ] || bench_skip "android-reset-bench.sh uses sips and adb emu screenshot on macOS"
android_env
image_dir "$image"
android_port "$port"
bench_start android-reset
emu_ready
builds=("$BENCH_EMU")
[ -n "$baseline" ] && builds+=("$baseline")
label() { if [ "$1" = "$BENCH_EMU" ] && [ -z "$baseline" ]; then echo emu; elif [ "$1" = "$BENCH_EMU" ]; then echo after; else echo before; fi; }
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
avd="bench-reset-$$"
avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd 720x1280@320
BENCH_ROUND=0
android_boot emu "$avd" "$port" "$BENCH_WORK/first.log" && bench_log "first boot ${BOOT_S}s (not counted)"
android_stop "$BOOT_PID" "$port"
read -r -a mode_list <<<"$modes"
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${builds[@]}")
  [ $((BENCH_ROUND % 2)) = 0 ] && [ ${#builds[@]} = 2 ] && order=("${builds[1]}" "${builds[0]}")
  for mode in "${mode_list[@]}"; do
    for bin in "${order[@]}"; do
      a=$(label "$bin")
      out=$(EMU=$bin PORT=$port ANDROID_HOME=$BENCH_SDK "$BENCH_REPO/scripts/android-reset-bench.sh" "$avd" "$mode" 1 2>/dev/null)
      line=$(printf '%s\n' "$out" | grep "^mode=$mode run=1" | tail -1)
      bench_log "$a: ${line:-no result: $(printf '%s' "$out" | tail -1)}"
      if [ "$mode" = exit ]; then
        bench_sample "$a" "exit: kill -INT to process exit, s" "$(field exit_s "$line")"
        bench_sample "$a" "exit: exit status" "$(field status "$line")"
      elif [ "$(field state "$line")" = booted ]; then
        bench_sample "$a" "$mode: trigger to the next boot's first host frame, s" "$(field first_frame_s "$line")"
        bench_sample "$a" "$mode: trigger to sys.boot_completed, s" "$(field boot_completed_s "$line")"
      else
        st=$(field state "$line")
        bench_sample "$a" "$mode: trigger to sys.boot_completed, s" "${st:-failed}"
      fi
    done
  done
done
bench_finish "before,after,emu" "Image: $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}); scripts/android-reset-bench.sh <avd> <mode> 1 per round and build."
