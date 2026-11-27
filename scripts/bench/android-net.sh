#!/usr/bin/env bash
# Android network: what emu's user-mode NAT costs an idle device, and what it carries.
#
#   scripts/bench/android-net.sh [--mode idle|throughput|both] [--image DIR] [--port N]
#                                [--rounds N] [--quick] [--out DIR]
#
# Creates an AVD (4 vCPUs, 2 GB), runs scripts/net-bench.sh on it and deletes it:
#   idle        one uncounted boot, then --rounds boots each with the NAT and with the link only
#               (EMU_NET=link), alternating; each waits 60 s after sys.boot_completed, then takes
#               emu's CPU over 60 s (% of one core)
#   throughput  one boot with the NAT, then 200 MiB each way between the guest's toybox nc and a
#               server on the host's loopback (the guest's 10.0.2.2), one warm-up then --rounds
#               transfers per direction, timed on the host by the server
# There is no stock comparison: the SDK emulator's slirp is not measured (docs/benchmarks.md,
# "Not yet measured").
#
# Defaults: --mode both, --image system-images/android-36/google_apis/arm64-v8a, --port 5720.
# --quick: one round, a 20 s settle and window, 50 MiB.
# Reproduces docs/benchmarks.md "Idle host CPU" (the NAT rows) and the throughput table.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
mode=both; image=system-images/android-36/google_apis/arm64-v8a; port=5720
while [ $# -gt 0 ]; do
  case $1 in
    --mode) mode=$2; shift 2 ;;
    --image) image=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    *) bench_opt "$@"; shift "$BENCH_SHIFT" ;;
  esac
done
case $mode in idle | throughput | both) ;; *) bench_die "--mode is idle, throughput or both" ;; esac
[ "$(uname -s)" = Darwin ] || bench_skip "scripts/net-bench.sh reads macOS's sysctl load average"
android_env
image_dir "$image"
android_port "$port"
bench_start android-net
emu_ready
avd="bench-net-$$"
avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048
quick_env=()
[ "$BENCH_QUICK" = 1 ] && quick_env=(SETTLE=20 WINDOW=20 MIB=50)
net_out="$BENCH_WORK/net"; mkdir -p "$net_out"
net_rc=0
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
if [ "$mode" != throughput ]; then
  env ${quick_env[@]+"${quick_env[@]}"} OUT="$net_out" PORT="$port" "$BENCH_REPO/scripts/net-bench.sh" idle "$avd" "$BENCH_ROUNDS" >&2 ||
    { net_rc=$?; bench_log "net-bench.sh idle exited $net_rc"; bench_sample "idle run" "net-bench.sh" failed; }
  while read -r line; do
    case $line in arm=*) ;; *) continue ;; esac
    BENCH_ROUND=$(field run "$line")
    case $(field arm "$line") in link) a="link-only virtio-net" ;; *) a="virtio-net with the NAT" ;; esac
    bench_sample "$a" "idle host CPU, % of a core" "$(field cpu_pct "$line")"
  done < "$net_out/net-idle.result"
fi
if [ "$mode" != idle ]; then
  env ${quick_env[@]+"${quick_env[@]}"} OUT="$net_out" PORT="$port" "$BENCH_REPO/scripts/net-bench.sh" throughput "$avd" "$BENCH_ROUNDS" >&2 ||
    { net_rc=$?; bench_log "net-bench.sh throughput exited $net_rc"; bench_sample "throughput run" "net-bench.sh" failed; }
  while read -r line; do
    case $line in *" run="*) ;; *) continue ;; esac
    BENCH_ROUND=$(field run "$line")
    case $(field dir "$line") in up) a="guest to host" ;; *) a="host to guest" ;; esac
    bench_sample "$a" "transfer time, s" "$(field secs "$line")"
    bench_sample "$a" "rate, MB/s" "$(field MBps "$line")"
  done < "$net_out/net-throughput.result"
fi
bench_finish "" "Image: $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}); scripts/net-bench.sh ${quick_env[*]-}"
[ "$net_rc" = 0 ] || exit "$net_rc"
