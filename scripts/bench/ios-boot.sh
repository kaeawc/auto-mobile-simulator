#!/usr/bin/env bash
# iOS boot: `simee boot` against `simctl`, a new device's first boot and a later one.
#
#   scripts/bench/ios-boot.sh [--device-type TYPE] [--runtime ID] [--simee BIN]
#                             [--rounds N] [--quick] [--out DIR]
#
# Each round, for each tool in turn (the order alternating between rounds), creates a new
# simulator (`simctl create`, so creation isn't timed), boots it and times the boot until it has
# finished booting, shuts it down, boots and times it again, shuts it down and deletes it:
#   simee   `simee boot <udid>` in its own process (SIMEE_IN_PROCESS=1), which returns once the
#           device has booted; headless, no Simulator.app
#   simctl  `xcrun simctl bootstatus <udid> -b`, which boots and waits for boot to finish
# Never opens or quits Simulator.app; touches only the devices it created.
#
# Defaults: --device-type "iPhone 17", --runtime the newest iOS runtime installed, --simee this
# checkout's release build (built here). --quick: one round.
# Reproduces docs/benchmarks.md "iOS: simee against simctl", the boot table.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
type="iPhone 17"; runtime=""
while [ $# -gt 0 ]; do
  case $1 in
    --device-type) type=$2; shift 2 ;;
    --runtime) runtime=$2; shift 2 ;;
    --simee) BENCH_SIMEE=$2; shift 2 ;;
    *) bench_opt "$@"; shift "$BENCH_SHIFT" ;;
  esac
done
ios_env
runtime=${runtime:-$(ios_runtime)}
[ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
bench_start ios-boot
simee_ready
boot() {
  local tool=$1 udid=$2 t0
  t0=$(now)
  if [ "$tool" = simee ]; then
    SIMEE_IN_PROCESS=1 "$BENCH_SIMEE" boot "$udid" --timeout 600 > "$BENCH_WORK/boot.out" 2>&1 || { cat "$BENCH_WORK/boot.out" >&2; echo failed; return; }
  else
    xcrun simctl bootstatus "$udid" -b > "$BENCH_WORK/boot.out" 2>&1 || { cat "$BENCH_WORK/boot.out" >&2; echo failed; return; }
  fi
  since "$t0" 1
}
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=(simee simctl)
  [ $((BENCH_ROUND % 2)) = 0 ] && order=(simctl simee)
  for tool in "${order[@]}"; do
    ios_create "simee-bench-boot-$$-$BENCH_ROUND-$tool" "$type" "$runtime"
    udid=$IOS_UDID
    bench_sample "$tool" "first boot of a new device, s" "$(boot "$tool" "$udid")"
    ios_shutdown "$udid"
    bench_sample "$tool" "later boot, s" "$(boot "$tool" "$udid")"
    ios_shutdown "$udid"
    ios_delete "$udid"
  done
done
bench_finish "simee,simctl" "$type, $runtime; each boot until the device has finished booting."
