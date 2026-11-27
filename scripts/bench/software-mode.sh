#!/usr/bin/env bash
# Android software mode (Linux without KVM): emu's devices under QEMU TCG against the SDK
# emulator's `-accel off`, boot to sys.boot_completed, idle host CPU and peak memory.
#
#   scripts/bench/software-mode.sh --qemu QEMU (or EMU_QEMU) [--sysdir DIR] [--lib64 DIR] [--icd swiftshader]
#                                  [--arms "emu stock"] [--budget S] [--rounds N] [--quick] [--out DIR]
#
# Each round runs scripts/tcg-bench.py once per arm with --boots 1, the order alternating:
#   emu    --emu-devices target/release/examples/soft_devices --lib64 <renderer lib64> --icd ...,
#          in a work directory kept for the run (its first boot formats userdata and isn't
#          counted, later rounds reuse it); emu:ARGS adds tcg-bench arguments, comma-separated,
#          for an A/B arm with its own work directory: emu:--vsync,30 or
#          emu:--append,kfence.sample_interval=0
#   tcg    plain QEMU TCG with stock's disk layout and no GPU (the CPU spike: root shell, zygote's
#          first start, guest CPU microbenchmarks); tcg:MODEL passes --cpu MODEL
#   stock  --stock <SDK>/emulator/emulator --avd <an AVD this script creates on --sysdir, 4 vCPUs,
#          2 GB>; tcg-bench makes one uncounted first boot per invocation, so a stock round costs
#          two boots (up to --budget s each, 2400)
# Per boot: seconds from QEMU's (or the emulator's) start to sys.boot_completed, host CPU in cores
# 60-120 s and 240-300 s after it, peak RSS. The AVD and the work directory are deleted at the end.
#
# Needs an x86_64 Linux host with no /dev/kvm, QEMU 11 or newer (--qemu), the renderer
# (`emu -provision-renderer`; --lib64 defaults to the newest under $ANDROID_HOME/simee/renderer),
# and the API 36 default x86_64 image (--sysdir, default
# $ANDROID_HOME/system-images/android-36/default/x86_64). The stock arm needs the SDK emulator.
# --quick: one round of emu only, --budget 900.
# Reproduces docs/benchmarks.md "Boot to sys.boot_completed with emu's devices" and the SDK
# emulator's software-mode row, the KFENCE and refresh-rate tables (emu:... arms), and the CPU
# model tables (--arms "tcg:max,-erms,-fsrm,-fzrm,-fsrs,-fsrc tcg:max": a model containing commas
# is one arm, as tcg takes everything after the colon; the --tb-size and --threads rows run
# scripts/tcg-bench.py directly).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
qemu=${EMU_QEMU:-}
sysdir=""
lib64=""
icd=swiftshader
arms=""
budget=""
while [ $# -gt 0 ]; do
  case $1 in
    --qemu)
      qemu=$2
      shift 2
      ;;
    --sysdir)
      sysdir=$2
      shift 2
      ;;
    --lib64)
      lib64=$2
      shift 2
      ;;
    --icd)
      icd=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --budget)
      budget=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && {
  arms=${arms:-emu}
  budget=${budget:-900}
}
arms=${arms:-emu stock}
budget=${budget:-2400}
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || bench_skip "software mode is measured on x86_64 Linux"
[ -e /dev/kvm ] && bench_skip "this host has /dev/kvm; software mode is for hosts without it"
[ -n "$qemu" ] && [ -x "$qemu" ] || bench_skip "needs --qemu or EMU_QEMU: qemu-system-x86_64, 11 or newer"
android_env
image_dir "${sysdir:-system-images/android-36/default/x86_64}"
if [ -z "$lib64" ]; then
  lib64=$(find "$BENCH_SDK/simee/renderer" -maxdepth 2 -type d -name lib64 2>/dev/null | sort | tail -1)
fi
[ -n "$lib64" ] && [ -d "$lib64" ] || bench_skip "no renderer lib64 (emu -provision-renderer)"
case " $arms " in *" stock "*) stock_ready ;; *) ;; esac
bench_start software-mode
emu_ready
(cd "$BENCH_REPO" && cargo build -q --release -p simee-emu-vmm --example soft_devices) || bench_die "building soft_devices failed"
soft="$BENCH_REPO/target/release/examples/soft_devices"
avd=""
case " $arms " in *" stock "*)
  avd="bench-soft-$$"
  avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048
  ;;
*) ;;
esac
read -r -a arm_list <<<"$arms"
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
slug() { printf '%s' "$1" | tr -c 'A-Za-z0-9' '_'; }
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arm_list[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arm_list[@]} - 1; i >= 0; i--)); do order+=("${arm_list[i]}"); done
  fi
  for arm in "${order[@]}"; do
    log="$BENCH_WORK/tcg-$BENCH_ROUND-$(slug "$arm").log"
    extra=()
    case $arm in *:*) IFS=, read -r -a extra <<<"${arm#*:}" ;; *) ;; esac
    case $arm in
      emu | emu:*)
        a="emu's devices"
        [ "$arm" = emu ] || a="emu's devices ${extra[*]}"
        python3 -I "$BENCH_REPO/scripts/tcg-bench.py" "$IMAGE_DIR" "$BENCH_WORK/work-$(slug "$arm")" --qemu "$qemu" \
          --emu-devices "$soft" --lib64 "$lib64" --icd "$icd" --boots 1 --budget "$budget" ${extra[@]+"${extra[@]}"} >"$log" 2>&1
        ;;
      tcg | tcg:*)
        cpu=()
        [ "$arm" = tcg ] || cpu=(--cpu "${arm#tcg:}")
        a="plain TCG${cpu[*]:+ ${cpu[*]}}"
        python3 -I "$BENCH_REPO/scripts/tcg-bench.py" "$IMAGE_DIR" "$BENCH_WORK/work-$(slug "$arm")" --qemu "$qemu" \
          ${cpu[@]+"${cpu[@]}"} --boots 1 --budget "$budget" >"$log" 2>&1
        ;;
      stock)
        a="SDK emulator -accel off"
        python3 -I "$BENCH_REPO/scripts/tcg-bench.py" "$IMAGE_DIR" "$BENCH_WORK/stock-work" --stock "$BENCH_STOCK" --avd "$avd" \
          --boots 1 --budget "$budget" >"$log" 2>&1
        ;;
      *) bench_die "arm $arm: expected emu, emu:ARGS, tcg, tcg:CPU or stock" ;;
    esac
    line=$(grep '^MEDIAN' "$log" | tail -1)
    bench_log "$a: ${line:-no result}"
    case $arm in
      tcg*)
        [ -n "$line" ] || {
          bench_sample "$a" "zygote starts (guest uptime), s" failed
          continue
        }
        bench_sample "$a" "root shell, s" "$(field shell_s "$line")"
        bench_sample "$a" "zygote starts (guest uptime), s" "$(field zygote_uptime_s "$line")"
        bench_sample "$a" "guest 100k shell loop, s" "$(field loop100k_s "$line")"
        bench_sample "$a" "guest 4x md5 of 64 MiB, s" "$(field md5x4_s "$line")"
        bench_sample "$a" "QEMU peak RSS, MB" "$(field peak_rss_mb "$line")"
        continue
        ;;
      *) ;;
    esac
    if grep -q '^failed boots: 1 of 1' "$log" || [ -z "$line" ]; then
      bench_sample "$a" "boot to sys.boot_completed, s" "over-$budget"
      continue
    fi
    bench_sample "$a" "boot to sys.boot_completed, s" "$(field boot_completed_s "$line")"
    bench_sample "$a" "host CPU 60-120 s after, cores" "$(field idle_cores "$line")"
    bench_sample "$a" "host CPU 240-300 s after, cores" "$(field settled_cores "$line")"
    if [ "$arm" = stock ]; then
      bench_sample "$a" "peak RSS, MB" "$(field peak_rss_mb "$line")"
    else
      bench_sample "$a" "peak RSS, MB" "$(awk -v q="$(field qemu_peak_rss_mb "$line")" -v e="$(field emu_peak_rss_mb "$line")" 'BEGIN { printf "%d", q + e }')"
    fi
  done
done
bench_finish "emu's devices,SDK emulator -accel off" "Image $IMAGE_DIR, renderer $lib64 on $icd, QEMU $qemu; scripts/tcg-bench.py --boots 1 per arm and round."
