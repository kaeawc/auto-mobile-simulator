#!/usr/bin/env bash
# Android farm: clones of one settled golden AVD booted together, until every one is settled.
#
#   scripts/bench/android-farm.sh [--sizes "1 4 8"] [--k K] [--image DIR] [--emu EMU]
#                                 [--port N] [--rounds N] [--quick] [--out DIR]
#
# Creates a golden AVD (4 vCPUs, 2 GB), boots it once under emu until sys.boot_completed plus 60 s
# (so its simee disks are settled), stops it, then for each farm size and round runs
# scripts/farm-bench-android.sh, which clones the golden (`emu -clone-avd`, APFS copy-on-write),
# boots the clones K at a time (default: all at once), waits until each is settled (boot complete
# and the emu process under 60% of a core over 5 s), measures them idle and deletes them. The
# golden is deleted at the end. Per run: seconds until all were settled, failures, host memory
# used per device (system-wide vm_stat delta / N), and the median device's idle CPU and footprint.
# No stock comparison yet (docs/benchmarks.md, "Not yet measured").
#
# Ports: device i gets console --port + 2(i-1) (default 5720); all must be free. Mind the host:
# --sizes 8 boots eight devices at once.
# Defaults: --sizes "1 4 8", --image system-images/android-36/google_apis/arm64-v8a.
# --quick: --sizes 1, one round.
# Reproduces docs/benchmarks.md "Android farm" (and docs/farm-android.md's benchmark).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
sizes=""; k=""; image=system-images/android-36/google_apis/arm64-v8a; port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --sizes) sizes=$2; shift 2 ;;
    --k) k=$2; shift 2 ;;
    --image) image=$2; shift 2 ;;
    --emu) BENCH_EMU=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    *) bench_opt "$@"; shift "$BENCH_SHIFT" ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && sizes=${sizes:-1}
sizes=${sizes:-1 4 8}
[ "$(uname -s)" = Darwin ] || bench_skip "scripts/farm-bench-android.sh uses macOS's vm_stat, footprint and top"
android_env
image_dir "$image"
max=0; for n in $sizes; do [ "$n" -gt "$max" ] && max=$n; done
for i in $(seq 0 $((max - 1))); do android_port $((port + 2 * i)); done
bench_start android-farm
emu_ready
golden="bench-farm-golden-$$"
avd_create "$golden" "$IMAGE_DIR" -cores 4 -memory 2048
BENCH_ROUND=0
android_boot emu "$golden" "$port" "$BENCH_WORK/golden.log" || bench_die "the golden AVD didn't boot; see $BENCH_WORK/golden.log"
bench_log "golden first boot ${BOOT_S}s; settling 60 s"
sleep 60
android_stop "$BOOT_PID" "$port"
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  for n in $sizes; do
    line=$(env EMU="$BENCH_EMU" EMU_TOOL="$BENCH_EMU" BASE_PORT="$port" OUT="$BENCH_WORK/farm" PREFIX="bench-farm-$$-" ${k:+K=$k} ANDROID_HOME="$BENCH_SDK" \
      "$BENCH_REPO/scripts/farm-bench-android.sh" "bench-$$-$BENCH_ROUND-$n" "$golden" "$n" 2> "$BENCH_WORK/farm-$BENCH_ROUND-$n.err" | grep '^label=' | tail -1)
    a="$n device$([ "$n" = 1 ] || echo s)"
    if [ -z "$line" ]; then bench_sample "$a" "all settled, s" failed; tail -3 "$BENCH_WORK/farm-$BENCH_ROUND-$n.err" >&2; continue; fi
    bench_sample "$a" "all settled, s" "$(field all_settled_s "$line")"
    bench_sample "$a" "failures" "$(field failures "$line")"
    bench_sample "$a" "host memory used per device, MB" "$(awk -v s="$(field sys_mb "$line")" -v n="$n" 'BEGIN { printf "%d", s / n }')"
    bench_sample "$a" "idle host CPU per device, % of a core (median device)" "$(field idle_cpu_pct_med "$line")"
    bench_sample "$a" "footprint per device, MB (median device)" "$(field footprint_mb_med "$line")"
  done
done
bench_finish "" "Clones of a settled golden ($IMAGE_DIR, 4 vCPUs, 2 GB, created ${AVD_MODE:-fresh} and booted here); scripts/farm-bench-android.sh MODE=clone${k:+ with K=$k}."
