#!/usr/bin/env bash
# iOS boot to settled: one fresh clone of a booted-once template at a time,
# `simee boot <clone> --profile slim --settle`, per build and profile.
#
#   scripts/bench/ios-settle.sh [--profiles "slim"] [--settle-cpu foreground|all] [--baseline SIMEE]
#                               [--device-type TYPE] [--runtime ID] [--simee BIN]
#                               [--rounds N] [--quick] [--out DIR]
#
# Creates a template simulator, boots it once until settled and shuts it down. Each round, for
# every build (this checkout's, and --baseline's for a before/after) and profile (`slim`, or
# `default` for none), in turn and alternating, clones the template (`simee clone`), boots the
# clone with that build's `simee boot --settle [--profile P] [--settle-cpu C]` in its own process,
# records simee's own timings, shuts the clone down and deletes it. The template is deleted at the
# end. Per boot: boot_s (until booted), settle_s (until the device's processes were under 10% of a
# core for 10 s), the profile's own time, CPU-seconds until settled, processes at settle.
#
# Defaults: --profiles slim, --settle-cpu foreground (simee's default), --device-type
# "iPhone 17 Pro", --runtime the newest iOS runtime. --quick: one round.
# Reproduces docs/benchmarks.md's slim-profile tables (iOS 27.2 and 18.0 before and after, with
# --runtime) and docs/farm.md's 62-against-74-job table (each build as --simee / --baseline).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
profiles=slim
settle_cpu=""
baseline=""
type="iPhone 17 Pro"
runtime=""
while [ $# -gt 0 ]; do
  case $1 in
    --profiles)
      profiles=$2
      shift 2
      ;;
    --settle-cpu)
      settle_cpu=$2
      shift 2
      ;;
    --baseline)
      baseline=$2
      shift 2
      ;;
    --device-type)
      type=$2
      shift 2
      ;;
    --runtime)
      runtime=$2
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
ios_env
runtime=${runtime:-$(ios_runtime)}
[ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
bench_start ios-settle
simee_ready
export SIMEE_IN_PROCESS=1
ios_create "simee-bench-settle-template-$$" "$type" "$runtime"
template=$IOS_UDID
"$BENCH_SIMEE" boot "$template" --settle --timeout 600 >&2 || bench_die "the template didn't boot"
ios_shutdown "$template" || bench_die "the template didn't shut down"
builds=("$BENCH_SIMEE")
[ -n "$baseline" ] && builds+=("$baseline")
read -r -a profile_list <<<"$profiles"
arms=()
for b in "${builds[@]}"; do for p in "${profile_list[@]}"; do arms+=("$b|$p"); done; done
label() {
  local b=${1%|*} p=${1#*|} who=""
  [ -n "$baseline" ] && { if [ "$b" = "$BENCH_SIMEE" ]; then who="after "; else who="before "; fi; }
  echo "$who$p"
}
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arms[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arms[@]} - 1; i >= 0; i--)); do order+=("${arms[i]}"); done
  fi
  for arm in "${order[@]}"; do
    bin=${arm%|*}
    p=${arm#*|}
    a=$(label "$arm")
    clone=$("$BENCH_SIMEE" clone "$template" "simee-bench-settle-$$-$BENCH_ROUND-$p" 2>"$BENCH_WORK/clone.err" | tail -1)
    case $clone in ????????-????-????-????-????????????) ;; *)
      cat "$BENCH_WORK/clone.err" >&2
      bench_die "simee clone failed"
      ;;
    esac
    bench_on_exit ios_delete "$clone"
    extra=()
    [ "$p" = default ] || extra=(--profile "$p")
    [ -n "$settle_cpu" ] && extra+=(--settle-cpu "$settle_cpu")
    line=$("$bin" boot "$clone" --settle --timeout 600 "${extra[@]}" 2>"$BENCH_WORK/boot.err" | tail -1)
    bench_log "$a: $line"
    if [ "$(field settled "$line")" = true ]; then
      bench_sample "$a" "booted until settled, s" "$(awk -v s="$(field settle_s "$line")" -v b="$(field boot_s "$line")" 'BEGIN { printf "%.1f", s - b }')"
    else
      bench_sample "$a" "booted until settled, s" "unsettled"
    fi
    bench_sample "$a" "launch until booted, s" "$(field boot_s "$line")"
    [ "$p" = default ] || bench_sample "$a" "the profile's own time, s" "$(field profile_s "$line")"
    bench_sample "$a" "CPU-s until settled" "$(field cpu_s "$line")"
    bench_sample "$a" "processes at settle" "$(field procs "$line")"
    ios_shutdown "$clone"
    ios_delete "$clone"
  done
done
bench_finish "" "Fresh clones of a booted-once $type ($runtime) template, one at a time; simee boot --settle${settle_cpu:+ --settle-cpu $settle_cpu}."
