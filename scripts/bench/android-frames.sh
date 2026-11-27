#!/usr/bin/env bash
# Android frame pacing under launcher swipes: emu against the SDK emulator, phone or tablet.
#
#   scripts/bench/android-frames.sh [--device phone|tablet] [--image DIR] [--arms "emu stock"]
#                                   [--settle S] [--runs N] [--port N] [--rounds N] [--quick] [--out DIR]
#
# One AVD of the device's size (phone 1080x2400@420, tablet 2560x1600@320; 4 vCPUs, 2 GB),
# deleted at the end. Every arm boots it once uncounted (first-boot work isn't frame pacing).
# Each round then warm-boots it under every arm in turn (order alternating), lets it settle
# --settle s (60) and runs scripts/android-jank.sh --runs times (1): 10 app-drawer swipes up and
# down on the launcher, then the launcher's own `dumpsys gfxinfo`. Per run: janky frames (%),
# frame-time p50/p90/p99 (ms), frames rendered, and the emulator process tree's host CPU over the
# swipes. The SDK emulator boots with -no-snapshot-load.
#
# Arms: emu, stock, or emu:K=V[,K=V] (an A/B arm of emu with environment settings).
# Defaults: --image system-images/android-36/google_apis/arm64-v8a, --port 5720.
# --quick: one round, --settle 20.
# Reproduces docs/benchmarks.md "Frame pacing". On Linux without KVM, emu boots the AVD in
# software mode (an x86_64 image) and this reproduces "Software mode through emu -avd"'s swipes.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
device=phone
image=""
arms="emu stock"
settle=""
runs=1
port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --device)
      device=$2
      shift 2
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --settle)
      settle=$2
      shift 2
      ;;
    --runs)
      runs=$2
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
case $device in
  phone) lcd=1080x2400@420 ;;
  tablet) lcd=2560x1600@320 ;;
  *) bench_die "--device is phone or tablet" ;;
esac
if [ -z "$image" ]; then
  if [ "$(uname -m)" = x86_64 ]; then image=system-images/android-36/default/x86_64; else image=system-images/android-36/google_apis/arm64-v8a; fi
fi
[ "$BENCH_QUICK" = 1 ] && settle=${settle:-20}
settle=${settle:-60}
android_env
image_dir "$image"
case " $arms " in *" stock "*) stock_ready ;; *) ;; esac
android_port "$port"
bench_start android-frames
emu_ready
read -r -a arm_list <<<"$arms"
label() { case $1 in stock) echo "SDK emulator" ;; *) echo "$1" ;; esac }
extra() { [ "${1%%:*}" = stock ] && echo -no-snapshot-load; }
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
avd="bench-frames-$$"
avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd "$lcd"
BENCH_ROUND=0
for arm in "${arm_list[@]}"; do
  # shellcheck disable=SC2046
  android_boot "$arm" "$avd" "$port" "$BENCH_WORK/first-${arm%%:*}.log" $(extra "$arm") && bench_log "$arm first boot ${BOOT_S}s (not counted)"
  android_stop "$BOOT_PID" "$port"
done

for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arm_list[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arm_list[@]} - 1; i >= 0; i--)); do order+=("${arm_list[i]}"); done
  fi
  for arm in "${order[@]}"; do
    a=$(label "$arm")
    # shellcheck disable=SC2046
    if ! android_boot "$arm" "$avd" "$port" "$BENCH_WORK/boot-$BENCH_ROUND-${arm%%:*}.log" $(extra "$arm"); then
      bench_sample "$a" "janky frames, %" "no-boot"
      android_stop "$BOOT_PID" "$port"
      continue
    fi
    sleep "$settle"
    for _ in $(seq 1 "$runs"); do
      line=$(ANDROID_HOME=$BENCH_SDK "$BENCH_REPO/scripts/android-jank.sh" "emulator-$port" "$BOOT_PID" 2>/dev/null | tail -1)
      bench_log "$arm: $line"
      bench_sample "$a" "janky frames, %" "$(field janky_pct "$line")"
      bench_sample "$a" "frame time p50, ms" "$(field p50 "$line")"
      bench_sample "$a" "frame time p90, ms" "$(field p90 "$line")"
      bench_sample "$a" "frame time p99, ms" "$(field p99 "$line")"
      bench_sample "$a" "frames rendered" "$(field frames "$line")"
      bench_sample "$a" "host CPU during the swipes, % of a core" "$(field host_cpu_pct "$line")"
    done
    android_stop "$BOOT_PID" "$port"
  done
done
bench_finish "emu,SDK emulator" "Device: $device ($lcd), image $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}), warm boots, ${settle} s settle, scripts/android-jank.sh."
