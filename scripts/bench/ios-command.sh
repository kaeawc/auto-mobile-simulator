#!/usr/bin/env bash
# iOS commands, one process per call: a screenshot and a tap through `simee` forwarded to
# `simee serve`, `simee` in its own process, and `simctl io screenshot`.
#
#   scripts/bench/ios-command.sh [--device-type TYPE] [--runtime ID] [--calls N] [--simee BIN]
#                                [--rounds N] [--quick] [--out DIR]
#
# Creates a simulator, boots it (`simctl bootstatus -b`, headless), waits 30 s for it to quieten,
# starts its own `simee serve` on a private socket, and then each round runs every arm --calls
# times (3) in turn, each call a new process timed from spawn to exit:
#   screenshot  `simee screenshot <udid> f.png` with SIMEE_SOCKET (forwarded to the server,
#               SIMEE_REQUIRE_SERVER=1), the same with SIMEE_IN_PROCESS=1, and
#               `xcrun simctl io <udid> screenshot f.png`
#   tap         `simee tap <udid> 600 2500` forwarded and in-process (simctl has no tap); on an
#               iPhone 17 that point is the gap between the dock's icons, so nothing changes
# Then stops the server and deletes the device.
#
# Defaults: --device-type "iPhone 17", --runtime the newest iOS runtime. --quick: one round,
# 1 call, a 10 s wait.
# Reproduces docs/benchmarks.md "iOS: simee against simctl" (one screenshot as a command) and
# docs/farm.md "Forwarding to simee serve".
set -uo pipefail
. "$(dirname "$0")/lib.sh"
type="iPhone 17"
runtime=""
calls=3
wait_s=30
while [ $# -gt 0 ]; do
  case $1 in
    --device-type)
      type=$2
      shift 2
      ;;
    --runtime)
      runtime=$2
      shift 2
      ;;
    --calls)
      calls=$2
      shift 2
      ;;
    --simee)
      BENCH_SIMEE=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && {
  calls=1
  wait_s=10
}
ios_env
runtime=${runtime:-$(ios_runtime)}
[ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
bench_start ios-command
simee_ready
ios_create "simee-bench-command-$$" "$type" "$runtime"
udid=$IOS_UDID
xcrun simctl bootstatus "$udid" -b >/dev/null || bench_die "simctl boot $udid failed"
sleep "$wait_s"
sock_dir
serve_start "$BENCH_SIMEE" "$SOCK_DIR/s.sock"
png="$BENCH_WORK/shot.png"
# Round 0 warms the server and each tool (the server's per-device worker, the HID client) and isn't
# recorded.
for BENCH_ROUND in 0 $(seq 1 "$BENCH_ROUNDS"); do
  [ "$BENCH_ROUND" = 0 ] && out=/dev/null || out=$BENCH_SAMPLES
  python3 -I - "$BENCH_ROUND" "$([ "$BENCH_ROUND" = 0 ] && echo 1 || echo "$calls")" "$BENCH_SIMEE" "$SOCK_DIR/s.sock" "$udid" "$png" >>"$out" <<'EOF'
import os, subprocess, sys, time
rnd, calls, simee, sock, udid, png = sys.argv[1], int(sys.argv[2]), *sys.argv[3:]
base = {k: v for k, v in os.environ.items() if not k.startswith('SIMEE_')}
served = dict(base, SIMEE_SOCKET=sock, SIMEE_REQUIRE_SERVER='1')
alone = dict(base, SIMEE_IN_PROCESS='1')
arms = [
    ('simee via simee serve', 'screenshot, ms', served, [simee, 'screenshot', udid, png]),
    ('simee in its own process', 'screenshot, ms', alone, [simee, 'screenshot', udid, png]),
    ('simctl', 'screenshot, ms', base, ['xcrun', 'simctl', 'io', udid, 'screenshot', png]),
    ('simee via simee serve', 'tap, ms', served, [simee, 'tap', udid, '600', '2500']),
    ('simee in its own process', 'tap, ms', alone, [simee, 'tap', udid, '600', '2500']),
]
for _ in range(calls):
    for arm, metric, env, argv in arms:
        t = time.perf_counter()
        r = subprocess.run(argv, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        ms = (time.perf_counter() - t) * 1000
        value = f'{ms:.1f}' if r.returncode == 0 else 'failed'
        if r.returncode:
            print(f'{arm} {argv[1]}: {r.stderr.decode().strip()}', file=sys.stderr)
        print(f'{rnd}\t{arm}\t{metric}\t{value}\t{os.getloadavg()[0]:.2f}')
EOF
  bench_log "round $BENCH_ROUND done"
done
bench_finish "simee via simee serve,simee in its own process,simctl" "$type, $runtime, booted headless; each call a new process, timed spawn to exit."
