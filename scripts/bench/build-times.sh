#!/usr/bin/env bash
# Build times of this checkout: clean builds into an empty target directory, in CPU-seconds.
#
#   scripts/bench/build-times.sh [--only "workspace desktop emu"] [--rounds N] [--quick] [--out DIR]
#
# Each round, for each build, a new empty CARGO_TARGET_DIR in the run's work directory (deleted
# after the build), then:
#   workspace  cargo test --no-run                 (the workspace's test build)
#   desktop    (cd desktop && cargo test --no-run)  (the desktop workspace: Rune's GPU stack)
#   emu        cargo build --release -p simee-emu         (the release emulator)
# Per build: CPU-seconds (user + system of cargo and every child, from getrusage), which load
# doesn't inflate, and wall seconds, which it does: read wall times with the load the JSON
# records. The dependencies must already be downloaded (cargo fetch); nothing is fetched here
# beyond what cargo does on its own.
#
# Defaults: all three, 3 rounds. --quick: the workspace build only, one round.
# Reproduces docs/benchmarks.md "Build times (local)" (the "after" column; "before" is the parent
# of the change that set the profiles, built the same way from its own checkout).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
only=""
while [ $# -gt 0 ]; do
  case $1 in
    --only)
      only=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && only=${only:-workspace}
only=${only:-workspace desktop emu}
command -v cargo >/dev/null || bench_skip "no cargo"
bench_start build-times
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  for b in $only; do
    target="$BENCH_WORK/target-$b"
    rm -rf "$target"
    case $b in
      workspace)
        dir=$BENCH_REPO
        cmd=(cargo test --no-run --quiet)
        ;;
      desktop)
        dir=$BENCH_REPO/desktop
        cmd=(cargo test --no-run --quiet)
        ;;
      emu)
        dir=$BENCH_REPO
        cmd=(cargo build --release --quiet -p simee-emu)
        ;;
      *) bench_die "unknown build $b" ;;
    esac
    read -r wall cpu rc <<<"$(cd "$dir" && CARGO_TARGET_DIR=$target cpu_timed "${cmd[@]}")"
    rm -rf "$target"
    if [ "$rc" != 0 ]; then
      tail -5 "$BENCH_WORK/cpu_timed.log" >&2
      bench_sample "$b" "clean build, CPU s" failed
      continue
    fi
    bench_sample "$b" "clean build, CPU s" "$cpu"
    bench_sample "$b" "clean build, wall s" "$wall"
  done
done
bench_finish "" "Clean builds into an empty CARGO_TARGET_DIR: workspace = cargo test --no-run, desktop = cd desktop && cargo test --no-run, emu = cargo build --release -p simee-emu."
