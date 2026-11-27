#!/usr/bin/env bash
# Time per automation step through `simee serve` (one persistent connection): tap, screenshot,
# and a tap followed by a screenshot. iOS by default; --android for an emu device.
#
#   scripts/bench/api-steps.sh [--device-type TYPE] [--runtime ID] [--calls N] [--baseline SIMEE]
#                              [--simee BIN] [--android [--image DIR] [--port N]]
#                              [--rounds N] [--quick] [--out DIR]
#
# Creates the device (iOS: `simctl create` + `simctl bootstatus -b`, then 30 s to quieten;
# --android: an AVD on --image booted under emu, then 60 s), and each round, for each build in
# turn (this checkout's, and --baseline's for a before/after, alternating), starts that build's
# `simee serve` on a private socket and runs scripts/ios-step-bench.py <socket> <device> --calls
# (20) against it: the median of --calls calls of each step, after one warm-up. The device is
# deleted at the end. iOS taps (600, 2500), an iPhone 17's dock gap; Android taps (360, 600) on
# an empty launcher workspace.
#
# Defaults: --device-type "iPhone 17", --runtime the newest iOS runtime; --android: --image
# system-images/android-36/google_apis/arm64-v8a, --port 5720. --quick: one round, 5 calls.
# Reproduces docs/benchmarks.md "iOS: simee against simctl" (time per automation step) and
# "Software mode through emu -avd" (tap and screenshot, --android on Linux without KVM).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
type="iPhone 17"; runtime=""; calls=20; baseline=""; android=0
image=system-images/android-36/google_apis/arm64-v8a; port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --device-type) type=$2; shift 2 ;;
    --runtime) runtime=$2; shift 2 ;;
    --calls) calls=$2; shift 2 ;;
    --baseline) baseline=$2; shift 2 ;;
    --simee) BENCH_SIMEE=$2; shift 2 ;;
    --android) android=1; shift ;;
    --image) image=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    *) bench_opt "$@"; shift "$BENCH_SHIFT" ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && calls=5
if [ "$android" = 1 ]; then
  android_env; image_dir "$image"; android_port "$port"; export ANDROID_HOME
else
  ios_env
  runtime=${runtime:-$(ios_runtime)}
  [ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
fi
bench_start api-steps
simee_ready
if [ "$android" = 1 ]; then
  emu_ready
  device="bench-steps-$$"
  avd_create "$device" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd 720x1280@320
  BENCH_ROUND=0
  android_boot emu "$device" "$port" "$BENCH_WORK/boot.log" || bench_die "the AVD didn't boot; see $BENCH_WORK/boot.log"
  sleep "$([ "$BENCH_QUICK" = 1 ] && echo 20 || echo 60)"
  xy=(360 600)
  what="$device (emu, $IMAGE_DIR, AVD created ${AVD_MODE:-fresh})"
else
  ios_create "simee-bench-steps-$$" "$type" "$runtime"
  device=$IOS_UDID
  xcrun simctl bootstatus "$device" -b > /dev/null || bench_die "simctl boot $device failed"
  sleep "$([ "$BENCH_QUICK" = 1 ] && echo 10 || echo 30)"
  xy=(600 2500)
  what="$type, $runtime"
fi
builds=("$BENCH_SIMEE")
[ -n "$baseline" ] && builds+=("$baseline")
label() { if [ -z "$baseline" ]; then echo simee; elif [ "$1" = "$BENCH_SIMEE" ]; then echo after; else echo before; fi; }
sock_dir
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${builds[@]}")
  [ $((BENCH_ROUND % 2)) = 0 ] && [ ${#builds[@]} = 2 ] && order=("${builds[1]}" "${builds[0]}")
  for bin in "${order[@]}"; do
    a=$(label "$bin")
    sock="$SOCK_DIR/$a.sock"
    rm -f "$sock"
    serve_start "$bin" "$sock"
    python3 -I "$BENCH_REPO/scripts/ios-step-bench.py" "$sock" "$device" "$calls" "${xy[@]}" > "$BENCH_WORK/steps.out" 2>&1
    bench_stop_pid "$SERVE_PID"
    grep -q 'median' "$BENCH_WORK/steps.out" || { cat "$BENCH_WORK/steps.out" >&2; bench_sample "$a" "tap, median ms" failed; continue; }
    while IFS= read -r line; do
      step=${line%%: median*}
      ms=$(printf '%s\n' "$line" | sed -n 's/.*: median \([0-9.]*\) ms.*/\1/p')
      [ -n "$ms" ] && bench_sample "$a" "$step, median ms" "$ms"
    done < "$BENCH_WORK/steps.out"
  done
done
[ "$android" = 1 ] && android_stop "$BOOT_PID" "$port"
bench_finish "before,after,simee" "$what; scripts/ios-step-bench.py, $calls calls per step after a warm-up, one connection."
