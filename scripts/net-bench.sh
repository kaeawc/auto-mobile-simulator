#!/usr/bin/env bash
# What emu's user-mode NAT costs an idle device and what it carries: one AVD, one emu build.
#
#   scripts/net-bench.sh idle <avd> [runs]        one boot to warm the AVD (not counted), then
#                                                 <runs> (3) boots each with the NAT and with the
#                                                 link only (EMU_NET=link), alternating
#   scripts/net-bench.sh throughput <avd> [runs]  one boot with the NAT, then <runs> (3) transfers
#                                                 each way after one warm-up transfer each way
#
# idle: waits SETTLE (60) s after sys.boot_completed, then takes emu's CPU time over WINDOW (60) s
# (`ps -o time`), as a percentage of one core, with the host's 1-minute load at both ends.
# throughput: MIB (200) MiB between the guest's toybox `nc` and a server on the host's loopback
# (the guest's 10.0.2.2), timed on the host by the server: from accept to the guest's last byte
# (guest to host), or from accept to the guest's close after the host's last byte (host to guest).
#
# Uses target/release/emu (built and signed here) on PORT (5600). BOOT_WRAP, if set, prefixes each
# boot (a host-wide boot gate, say); the boot returns once the guest has booted. Each instance is
# stopped by its PID (kill -INT). Results go to stdout and $OUT/net-<mode>.result (OUT defaults to
# bench/); the PID file is per run ($OUT/net-bench-<run>.pid), so runs on other ports are independent.
set -uo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
adb="$sdk/platform-tools/adb"
port=${PORT:-5600}; serial="emulator-$port"
emu="$repo/target/release/emu"
# OUT: where the log, result and pid files go (default bench/). The pidfile is named for this run
# (NET_BENCH_RUN, exported so the _boot child shares it), so two runs never read each other's PID.
out=${OUT:-$repo/bench}; mkdir -p "$out"
NET_BENCH_RUN=${NET_BENCH_RUN:-$$}; export NET_BENCH_RUN
pidfile="$out/net-bench-$NET_BENCH_RUN.pid"

# Internal: start emu (arm: nat or link) and return once the guest has booted, its PID in $pidfile.
if [ "${1:-}" = _boot ]; then
  arm=$2 avd=$3
  net=""; if [ "$arm" = link ]; then net="link"; fi
  env -i HOME="$HOME" USER="$USER" TMPDIR="$TMPDIR" PATH="/usr/bin:/bin:/usr/sbin:/sbin" ANDROID_HOME="$sdk" \
    ANDROID_SDK_ROOT="$sdk" EMU_NET="$net" \
    "$emu" -avd "$avd" -port "$port" -no-window -no-audio -no-boot-anim -no-snapshot-save > "$out/net-bench-$NET_BENCH_RUN-$arm.log" 2>&1 &
  echo $! > "$pidfile"
  deadline=$(( SECONDS + ${BOOT_TIMEOUT:-600} ))
  while [ "$SECONDS" -lt "$deadline" ]; do
    kill -0 "$(cat "$pidfile")" 2>/dev/null || { echo "emu exited; see $out/net-bench-$NET_BENCH_RUN-$arm.log" >&2; exit 1; }
    [ "$("$adb" -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && exit 0
    sleep 1
  done
  echo "no boot in ${BOOT_TIMEOUT:-600} s" >&2; exit 1
fi

mode=${1:?usage: scripts/net-bench.sh idle|throughput <avd> [runs]}
avd=${2:?avd}; runs=${3:-3}
(cd "$repo" && cargo build -q --release -p simee-emu) && codesign -s - -f --entitlements "$repo/scripts/hypervisor.entitlements" "$emu" 2>/dev/null
result="$out/net-$mode.result"; : > "$result"
say() { echo "$*" | tee -a "$result"; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
# `ps -o time` reads [[H:]M]M:SS.ss; seconds.
cpu_s() { ps -o time= -p "$1" | awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }'; }

pid=""
stop() {
  [ -n "$pid" ] || return 0
  kill -INT "$pid" 2>/dev/null
  for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
  pid=""
}
trap 'stop; rm -f "$pidfile"' EXIT
boot() {
  # BOOT_WRAP is a command prefix: split on purpose.
  # shellcheck disable=SC2086
  ${BOOT_WRAP:-} "$0" _boot "$1" "$avd" || exit 1
  pid=$(cat "$pidfile")
}

case "$mode" in
  idle)
    settle=${SETTLE:-60} window=${WINDOW:-60}
    boot nat; stop # warm-up: first-boot work (userdata, dex) isn't idle
    for i in $(seq "$runs"); do
      for arm in link nat; do
        boot "$arm"
        sleep "$settle"
        l0=$(load1); c0=$(cpu_s "$pid"); sleep "$window"; c1=$(cpu_s "$pid"); l1=$(load1)
        stop
        say "$(awk -v a="$arm" -v i="$i" -v c0="$c0" -v c1="$c1" -v w="$window" -v l0="$l0" -v l1="$l1" \
          'BEGIN { printf "arm=%s run=%d cpu_pct=%.1f load1=%s..%s\n", a, i, 100 * (c1 - c0) / w, l0, l1 }')"
      done
    done
    for arm in link nat; do
      grep "arm=$arm " "$result" | sed 's/.*cpu_pct=\([0-9.]*\).*/\1/' | sort -n | awk -v a="$arm" '{ v[NR] = $1 } END { printf "%s median=%.1f%% of a core (n=%d)\n", a, v[int((NR + 1) / 2)], NR }' | tee -a "$result"
    done
    ;;
  throughput)
    mib=${MIB:-200}
    # One-shot server: prints the seconds from accept to the end of the transfer.
    serve() {
      python3 -I -c '
import socket, sys, time
mode, n = sys.argv[1], int(sys.argv[2]) << 20
l = socket.create_server(("127.0.0.1", 0)); print(l.getsockname()[1], flush=True)
s, _ = l.accept(); t = time.monotonic(); got = 0
if mode == "sink":
    while (b := s.recv(1 << 20)): got += len(b)
else:
    chunk = bytes(1 << 20)
    for _ in range(n >> 20): s.sendall(chunk)
    got = n; s.shutdown(socket.SHUT_WR)
    while s.recv(1 << 16): pass
print(f"{time.monotonic() - t:.3f} {got}", flush=True)
' "$@"
    }
    boot nat
    sleep "${SETTLE:-30}"
    for dir in up down; do
      for i in $(seq 0 "$runs"); do
        fifo=$(mktemp -u); mkfifo "$fifo"
        if [ "$dir" = up ]; then serve sink "$mib" > "$fifo" & else serve source "$mib" > "$fifo" & fi
        exec 3< "$fifo"; read -r p <&3
        if [ "$dir" = up ]; then
          # nc shuts its sending half at dd's EOF: the server's read ends at the last byte.
          "$adb" -s "$serial" shell "dd if=/dev/zero bs=1048576 count=$mib 2>/dev/null | nc 10.0.2.2 $p" > /dev/null
        else
          # stdin held open, so nc closes only at the host's EOF, after the last byte.
          "$adb" -s "$serial" shell "sleep 10 | nc 10.0.2.2 $p > /dev/null"
        fi
        read -r secs got <&3; exec 3<&-; rm -f "$fifo"; wait
        [ "$got" = $((mib << 20)) ] || say "dir=$dir run=$i short: $got bytes"
        tag=run; [ "$i" = 0 ] && tag=warmup
        say "dir=$dir $tag=$i secs=$secs MBps=$(awk -v s="$secs" -v m="$mib" 'BEGIN { printf "%.0f", m * 1.048576 / s }') load1=$(load1)"
      done
      grep "dir=$dir run=" "$result" | sed 's/.*secs=\([0-9.]*\).*/\1/' | sort -n | awk -v d="$dir" -v m="$mib" '{ v[NR] = $1 } END { s = v[int((NR + 1) / 2)]; printf "%s median=%.3f s (%.0f MB/s, n=%d)\n", d, s, m * 1.048576 / s, NR }' | tee -a "$result"
    done
    ;;
  *) echo "mode must be idle or throughput" >&2; exit 2 ;;
esac
