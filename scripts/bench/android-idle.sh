#!/usr/bin/env bash
# Android first boots and idle host CPU: emu against the SDK emulator on one new AVD.
#
#   scripts/bench/android-idle.sh [--image DIR] [--lcd WxH@DPI] [--arms "emu stock"] [--warm]
#                                 [--launcher] [--settle S] [--window S] [--port N]
#                                 [--rounds N] [--quick] [--out DIR]
#
# One AVD (emu -create-avd, 4 vCPUs, 2 GB) for the whole run, deleted at the end. Each round boots
# it under every arm in turn (the order alternating between rounds), each boot a first boot
# (-wipe-data; --warm: no wipe, after one uncounted boot per arm), with -no-window -no-audio
# -no-boot-anim -no-snapshot-save. Per boot: seconds from launch until sys.boot_completed (polled
# every 0.25 s); with --launcher, until the launcher has focus (`dumpsys window`, polled each
# second); then, unless --window 0, the device sits --settle s (120) and idle host CPU is the
# emulator's process tree's CPU time over the next --window s (120), as % of one core.
#
# Arms: emu, stock (the SDK emulator), or emu:K=V[,K=V] for emu with environment settings, e.g.
# --arms "emu emu:ANGLE_DEFAULT_PLATFORM=metal" for emu's host GLES backends (API 29 first boots).
#
# Defaults: --image system-images/android-29/default/arm64-v8a, --lcd 720x1280@320, --port 5720.
# --quick: one round, --settle 20 --window 20.
# Reproduces docs/benchmarks.md "API 29 and 30: first boots and idle host CPU" (and, with
# --arms "emu emu:ANGLE_DEFAULT_PLATFORM=metal" --launcher --window 0, the host GLES table).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
image=system-images/android-29/default/arm64-v8a
lcd=720x1280@320
arms="emu stock"
warm=0
launcher=0
settle=""
window=""
port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --image)
      image=$2
      shift 2
      ;;
    --lcd)
      lcd=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --warm)
      warm=1
      shift
      ;;
    --launcher)
      launcher=1
      shift
      ;;
    --settle)
      settle=$2
      shift 2
      ;;
    --window)
      window=$2
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
if [ "$BENCH_QUICK" = 1 ]; then
  settle=${settle:-20}
  window=${window:-20}
fi
settle=${settle:-120}
window=${window:-120}
android_env
image_dir "$image"
case " $arms " in *" stock "*) stock_ready ;; *) ;; esac
android_port "$port"
bench_start android-idle
emu_ready
read -r -a arm_list <<<"$arms"
label() { case $1 in stock) echo "SDK emulator" ;; *) echo "$1" ;; esac }
avd="bench-idle-$$"
avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd "$lcd"
kind="first boot"
flags=(-wipe-data)
if [ "$warm" = 1 ]; then
  kind="warm boot"
  flags=()
  for arm in "${arm_list[@]}"; do
    BENCH_ROUND=0
    android_boot "$arm" "$avd" "$port" "$BENCH_WORK/warmup-${arm%%:*}.log" && bench_log "$arm warm-up boot ${BOOT_S}s (not counted)"
    android_stop "$BOOT_PID" "$port"
  done
fi

for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arm_list[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arm_list[@]} - 1; i >= 0; i--)); do order+=("${arm_list[i]}"); done
  fi
  for arm in "${order[@]}"; do
    a=$(label "$arm")
    t0=$(now)
    if ! android_boot "$arm" "$avd" "$port" "$BENCH_WORK/boot-$BENCH_ROUND-${arm%%:*}.log" ${flags[@]+"${flags[@]}"}; then
      bench_sample "$a" "$kind: launch until sys.boot_completed, s" never
      android_stop "$BOOT_PID" "$port"
      continue
    fi
    bench_sample "$a" "$kind: launch until sys.boot_completed, s" "$BOOT_S"
    [ "$launcher" = 1 ] && bench_sample "$a" "$kind: launch until the launcher has focus, s" "$(launcher_s "emulator-$port" "$t0" 120)"
    if [ "$window" != 0 ]; then
      sleep "$settle"
      c0=$(cpu_s "$BOOT_PID")
      w0=$(now)
      sleep "$window"
      c1=$(cpu_s "$BOOT_PID")
      w=$(since "$w0" 3)
      bench_sample "$a" "idle host CPU, % of a core (${settle} s after boot, over ${window} s)" \
        "$(awk -v a="$c0" -v b="$c1" -v w="$w" 'BEGIN { printf "%.1f", 100 * (b - a) / w }')"
    fi
    android_stop "$BOOT_PID" "$port"
  done
done
bench_finish "emu,SDK emulator" "Image: $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}), AVD 4 vCPUs, 2 GB, $lcd; boots with -no-window -no-audio -no-boot-anim -no-snapshot-save ${flags[*]-}."
