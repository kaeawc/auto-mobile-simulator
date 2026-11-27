#!/usr/bin/env bash
# Android boot: emu against the SDK emulator on one new AVD, cold and warm.
#
#   scripts/bench/android-boot.sh [--image DIR] [--lcd WxH@DPI] [--arms "emu stock"] [--snapshot]
#                                 [--port N] [--rounds N] [--quick] [--out DIR]
#
# Each round creates an AVD (emu -create-avd, 4 vCPUs, 2 GB) and boots it with
# scripts/android-bench.sh, the arms' order alternating from round to round:
#   cold   each arm's first boot of the new AVD (emu formats and encrypts its own /data; the SDK
#          emulator creates its userdata-qemu.img)
#   warm   a later boot of the same AVD; the SDK emulator with -no-snapshot-load, so both do a full
#          boot (emu has no snapshots)
# then deletes the AVD. Per boot: seconds from launch until adb lists the device and until
# sys.boot_completed, the process tree's peak RSS; on warm boots also android-bench.sh's guest CPU
# microbenchmarks (a 100k-iteration shell loop, 4 parallel md5sums of 64 MiB) and `adb shell echo`.
# --snapshot adds the SDK emulator restoring its quickboot snapshot (one boot that saves it, then
# the measured boot), for reference.
#
# Arms: emu, stock, or emu:K=V[,K=V] for an A/B arm of emu with EMU_* settings (android-bench.sh
# passes only EMU_* through), e.g. --arms "emu:EMU_VCPU_WAKE=targeted emu:EMU_VCPU_WAKE=none".
#
# Defaults: --image system-images/android-36/google_apis/arm64-v8a, --port 5720 (adb 5721).
# Needs the SDK emulator for the stock arm and an installed image. Results: bench/results/.
# Reproduces docs/benchmarks.md "Android: emu against the SDK emulator" and its API 29 table
# (--image system-images/android-29/default/arm64-v8a --lcd 720x1280@320).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
image=system-images/android-36/google_apis/arm64-v8a
lcd=""
arms="emu stock"
snapshot=0
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
    --snapshot)
      snapshot=1
      shift
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
android_env
image_dir "$image"
case " $arms " in *" stock "*) stock_ready ;; *) ;; esac
[ "$snapshot" = 1 ] && stock_ready
android_port "$port"
bench_start android-boot
emu_ready

read -r -a arm_list <<<"$arms"
label() { case $1 in stock) echo "SDK emulator" ;; *) echo "$1" ;; esac }
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
# One boot through android-bench.sh; records its numbers under PHASE.
bench_boot() {
  local arm=$1 avd=$2 phase=$3 kind=emu envs=() line state extra=()
  shift 3
  case $arm in
    stock) kind=stock ;;
    emu:*) IFS=, read -r -a envs <<<"${arm#emu:}" ;;
    *) ;;
  esac
  [ "$phase" = warm ] && [ "$kind" = stock ] && extra=(-no-snapshot-load)
  line=$(env ${envs[@]+"${envs[@]}"} PORT="$port" "$BENCH_REPO/scripts/android-bench.sh" \
    "bench-$BENCH_ROUND-$phase-${arm%%:*}-$$" "$avd" "$kind" ${extra[@]+"${extra[@]}"} | tail -1)
  state=$(field state "$line")
  local a
  a=$(label "$arm")
  if [ "$state" != booted ]; then
    bench_sample "$a" "$phase: launch until sys.boot_completed, s" "${state:-failed}"
    return 1
  fi
  bench_sample "$a" "$phase: launch until sys.boot_completed, s" "$(field boot_s "$line")"
  bench_sample "$a" "$phase: launch until adb lists the device, s" "$(field adb_s "$line")"
  bench_sample "$a" "$phase: peak RSS of the process tree, MB" "$(field peak_rss_mb "$line")"
  if [ "$phase" = warm ]; then
    bench_sample "$a" "warm: adb shell echo, s" "$(field echo "$line")"
    bench_sample "$a" "warm: guest 100k shell loop, s" "$(field loop100k "$line")"
    bench_sample "$a" "warm: guest 4x md5 of 64 MiB, s" "$(field md5x4 "$line")"
  fi
  [ "$(field starved "$line")" = yes ] && bench_log "round $BENCH_ROUND $arm $phase: load above the CPU count (starved=yes)"
  return 0
}

for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  avd="bench-boot-$$-$BENCH_ROUND"
  avd_args=(-cores 4 -memory 2048)
  [ -n "$lcd" ] && avd_args+=(-lcd "$lcd")
  avd_create "$avd" "$IMAGE_DIR" "${avd_args[@]}"
  order=("${arm_list[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arm_list[@]} - 1; i >= 0; i--)); do order+=("${arm_list[i]}"); done
  fi
  for arm in "${order[@]}"; do bench_boot "$arm" "$avd" cold; done
  for arm in "${order[@]}"; do bench_boot "$arm" "$avd" warm; done
  if [ "$snapshot" = 1 ]; then
    if SNAPSHOT_SAVE=1 android_boot stock "$avd" "$port" "$BENCH_WORK/snap-save.log"; then
      android_stop "$BOOT_PID" "$port"
      if android_boot stock "$avd" "$port" "$BENCH_WORK/snap-load.log"; then
        bench_sample "SDK emulator" "snapshot restore until sys.boot_completed, s" "$BOOT_S"
      fi
      android_stop "$BOOT_PID" "$port"
    fi
  fi
  avd_delete "$avd"
done
bench_finish "emu,SDK emulator" "Image: $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}). Boots with -no-window -no-audio -no-boot-anim -no-snapshot-save (scripts/android-bench.sh); per-boot logs in bench/."
