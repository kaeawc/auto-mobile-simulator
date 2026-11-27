#!/usr/bin/env bash
# iOS farm: clones of one booted-once template booted K at a time, with and without the slim
# profile, until every one is settled.
#
#   scripts/bench/ios-farm.sh [--n N] [--k K] [--profiles "default slim"] [--device-type TYPE]
#                             [--runtime ID] [--simee BIN] [--rounds N] [--quick] [--out DIR]
#
# Creates a template simulator, boots it once until settled (`simee boot --settle`) and shuts it
# down. Each round, for each profile in turn (order alternating), runs scripts/farm-bench-ios.sh
# with MODE=clone BOOT=many: it clones the template N times (`simee clone`), boots the clones with
# `simee boot-many -j K` (with --profile slim for the slim arm), judges each settled from outside
# (its launchd_sim tree under 10% of a core for 10 s), measures them and deletes them. The
# template is deleted at the end. Per run: seconds until all settled (or how many didn't within
# 600 s), footprint and processes per device, the devices' CPU 60 s after settling.
#
# Defaults: --n 8, --k 4, --device-type "iPhone 17", --runtime the newest iOS runtime.
# --quick: --n 1, one round. Every simee call runs in its own process (SIMEE_IN_PROCESS=1).
# Reproduces docs/benchmarks.md "iOS: simee against simctl", the farm table (default against
# slim), and docs/farm.md's settle tables.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
n=""; k=4; profiles="default slim"; type="iPhone 17"; runtime=""
while [ $# -gt 0 ]; do
  case $1 in
    --n) n=$2; shift 2 ;;
    --k) k=$2; shift 2 ;;
    --profiles) profiles=$2; shift 2 ;;
    --device-type) type=$2; shift 2 ;;
    --runtime) runtime=$2; shift 2 ;;
    --simee) BENCH_SIMEE=$2; shift 2 ;;
    *) bench_opt "$@"; shift "$BENCH_SHIFT" ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && n=${n:-1}
n=${n:-8}
ios_env
runtime=${runtime:-$(ios_runtime)}
[ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
bench_start ios-farm
simee_ready
export SIMEE_IN_PROCESS=1
# On any exit (Ctrl-C included) shut down and delete the clones this run's farm-bench-ios.sh made,
# found by this run's own $$-unique name prefix, by UDID. Registered before the template so it
# runs first (cleanup is in reverse order). Other simulators are never matched.
farm_prefix="simee-bench-farm-$$-"
sweep_clones() {
  local u
  for u in $(xcrun simctl list devices -j | python3 -I -c '
import json, sys
for devs in json.load(sys.stdin)["devices"].values():
    for d in devs:
        if d["name"].startswith(sys.argv[1]):
            print(d["udid"])' "$farm_prefix"); do
    ios_delete "$u"
  done
}
bench_on_exit sweep_clones
ios_create "simee-bench-farm-template-$$" "$type" "$runtime"
template=$IOS_UDID
"$BENCH_SIMEE" boot "$template" --settle --timeout 600 >&2 || bench_die "the template didn't boot"
ios_shutdown "$template" || bench_die "the template didn't shut down"
read -r -a profile_list <<< "$profiles"
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${profile_list[@]}")
  [ $((BENCH_ROUND % 2)) = 0 ] && [ ${#profile_list[@]} = 2 ] && order=("${profile_list[1]}" "${profile_list[0]}")
  for p in "${order[@]}"; do
    args=""; [ "$p" = default ] || args="--profile $p"
    line=$(SIMEE="$BENCH_SIMEE" LABEL="r$BENCH_ROUND-$p" PREFIX="$farm_prefix" MODE=clone TEMPLATE="$template" \
      BOOT=many K="$k" BOOT_ARGS="$args" DEVICE_TYPE="$type" RUNTIME="$runtime" \
      "$BENCH_REPO/scripts/farm-bench-ios.sh" "$n" 2> "$BENCH_WORK/farm-$BENCH_ROUND-$p.err" | grep '^label=' | tail -1)
    if [ -z "$line" ]; then tail -3 "$BENCH_WORK/farm-$BENCH_ROUND-$p.err" >&2; bench_sample "$p" "all settled, s" failed; continue; fi
    unsettled=$(field unsettled "$line")
    if [ "${unsettled:-1}" = 0 ]; then
      bench_sample "$p" "all settled, s" "$(field all_settled_s "$line")"
    else
      bench_sample "$p" "all settled, s" "$((n - unsettled))-of-$n"
    fi
    bench_sample "$p" "footprint per device, MB" "$(field fp_mb_per "$line")"
    bench_sample "$p" "processes per device" "$(field procs_per "$line")"
    bench_sample "$p" "devices' CPU 60 s after settling, % of a core" "$(field cpu_pct_idle60s "$line")"
  done
done
bench_finish "default,slim" "$n clones of a booted-once $type ($runtime) template, simee boot-many -j $k; scripts/farm-bench-ios.sh MODE=clone BOOT=many."
