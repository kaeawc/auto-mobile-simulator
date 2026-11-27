#!/usr/bin/env bash
# Android device bundles: `simee export` of a stopped AVD and `simee import` under a new name.
#
#   scripts/bench/android-bundle.sh [--image DIR] [--uptime S] [--verify] [--port N]
#                                   [--rounds N] [--quick] [--out DIR]
#
# Each round creates an AVD (4 vCPUs, 2 GB), boots it once under emu, leaves it up --uptime s (100)
# with a file written to /data/local/tmp, stops it, exports it to a tar in the run's work
# directory, imports that under a new name (streamed, every hash checked), and deletes both AVDs
# and the tar. Per round: the disks' logical and allocated size and the bundle's (from export's
# summary), export and import wall time, import's peak RSS. --verify also boots the import and
# checks the file is there. No stock tool exports an AVD, so there is no comparison.
#
# Defaults: --image system-images/android-36/google_apis/arm64-v8a, --port 5720.
# --quick: one round, --uptime 20.
# Reproduces docs/benchmarks.md "Android: device bundles" (the LZ4 column; zstd is not shipped).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
image=system-images/android-36/google_apis/arm64-v8a
uptime=""
verify=0
port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --image)
      image=$2
      shift 2
      ;;
    --uptime)
      uptime=$2
      shift 2
      ;;
    --verify)
      verify=1
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
[ "$BENCH_QUICK" = 1 ] && uptime=${uptime:-20}
uptime=${uptime:-100}
android_env
image_dir "$image"
android_port "$port"
bench_start android-bundle
emu_ready
simee_ready
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
mb() { awk -v b="$1" 'BEGIN { printf "%d", b / 1000000 }'; }
# Wall time and peak RSS (MB) of a command: "seconds rss_mb" on the last line of $BENCH_WORK/timed.
timed() {
  local t0 rss
  t0=$(now)
  if [ "$(uname -s)" = Darwin ]; then
    /usr/bin/time -l "$@" >"$BENCH_WORK/timed.out" 2>"$BENCH_WORK/timed.err"
    rss=$(awk '/maximum resident set size/ { printf "%d", $1 / 1048576 }' "$BENCH_WORK/timed.err")
  else
    /usr/bin/time -v "$@" >"$BENCH_WORK/timed.out" 2>"$BENCH_WORK/timed.err"
    rss=$(awk -F': ' '/Maximum resident set size/ { printf "%d", $2 / 1024 }' "$BENCH_WORK/timed.err")
  fi
  echo "$(since "$t0") ${rss:-}"
}
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  avd="bench-bundle-$$-$BENCH_ROUND"
  imp="bench-bundle-$$-$BENCH_ROUND-imported"
  avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048
  bench_on_exit avd_delete "$imp"
  android_boot emu "$avd" "$port" "$BENCH_WORK/boot-$BENCH_ROUND.log" || {
    bench_sample lz4 "export, s" no-boot
    android_stop "$BOOT_PID" "$port"
    continue
  }
  "$BENCH_ADB" -s "emulator-$port" shell "echo simee-bench-$BENCH_ROUND > /data/local/tmp/bench.txt"
  sleep "$uptime"
  android_stop "$BOOT_PID" "$port"
  tar="$BENCH_WORK/$avd.tar"
  read -r secs _ <<<"$(timed env ANDROID_HOME="$BENCH_SDK" "$BENCH_SIMEE" export "$avd" "$tar")"
  line=$(tail -1 "$BENCH_WORK/timed.out")
  [ -s "$tar" ] || {
    cat "$BENCH_WORK/timed.err" >&2
    bench_sample lz4 "export, s" failed
    continue
  }
  bench_sample lz4 "disks' logical size, MB" "$(mb "$(field disk_bytes "$line")")"
  bench_sample lz4 "disks' allocated size, MB" "$(mb "$(field allocated_bytes "$line")")"
  bench_sample lz4 "bundle, MB" "$(mb "$(wc -c <"$tar")")"
  bench_sample lz4 "export, wall s" "$secs"
  read -r secs rss <<<"$(timed env ANDROID_HOME="$BENCH_SDK" "$BENCH_SIMEE" import "$tar" "$imp")"
  if grep -q "$imp" "$BENCH_WORK/timed.out"; then
    bench_sample lz4 "import, wall s" "$secs"
    bench_sample lz4 "import, peak RSS MB" "${rss:-}"
  else
    cat "$BENCH_WORK/timed.err" >&2
    bench_sample lz4 "import, wall s" failed
  fi
  rm -f "$tar"
  if [ "$verify" = 1 ] && android_boot emu "$imp" "$port" "$BENCH_WORK/verify-$BENCH_ROUND.log"; then
    got=$("$BENCH_ADB" -s "emulator-$port" shell cat /data/local/tmp/bench.txt 2>/dev/null | tr -d '\r')
    bench_sample lz4 "the file is in the import" "$([ "$got" = "simee-bench-$BENCH_ROUND" ] && echo yes || echo no)"
  fi
  [ "$verify" = 1 ] && android_stop "$BOOT_PID" "$port"
  avd_delete "$imp"
  avd_delete "$avd"
done
bench_finish "" "Image: $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}); one first boot, ${uptime} s up, then simee export / simee import."
