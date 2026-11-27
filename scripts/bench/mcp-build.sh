#!/usr/bin/env bash
# What the MCP server's protocol layer costs to build: simee's hand-written sim-mcp against the
# official Rust SDK (rmcp).
#
#   scripts/bench/mcp-build.sh [--rmcp VERSION] [--rounds N] [--quick] [--out DIR]
#
#   sim-mcp  sim-mcp's direct dependencies that aren't this workspace's own crates
#            (`cargo tree -p sim-mcp -e normal --depth 1`), and the packages in Cargo.lock
#   rmcp     a scratch package in the run's work directory (the repo's rust-toolchain.toml, an
#            empty main) depending on rmcp VERSION (3.11) with default-features = false,
#            features ["server", "transport-io"], and tokio with rt, macros and io-std: the crates
#            in its normal tree, then --rounds clean dev builds with CARGO_BUILD_JOBS=2 (wall and
#            CPU-seconds). This downloads rmcp and its dependencies from crates.io into cargo's
#            own cache; nothing is added to this repository.
# --quick: the sim-mcp counts only (no download or build).
# Reproduces docs/benchmarks.md "MCP server".
set -uo pipefail
. "$(dirname "$0")/lib.sh"
rmcp=3.11
while [ $# -gt 0 ]; do
  case $1 in
    --rmcp)
      rmcp=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
command -v cargo >/dev/null || bench_skip "no cargo"
bench_start mcp-build
BENCH_ROUND=1
cd "$BENCH_REPO" || exit 2
members=$(cargo metadata --no-deps --format-version 1 | python3 -I -c 'import json, sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"]))')
deps=$(cargo tree -p sim-mcp -e normal --depth 1 --prefix none 2>/dev/null | awk '{ print $1 }' | sort -u | grep -vxF -f <(printf '%s\n' "$members"))
bench_log "sim-mcp's direct dependencies outside the workspace: $(printf '%s' "$deps" | tr '\n' ' ')"
bench_sample sim-mcp "direct dependencies outside the workspace" "$(printf '%s\n' "$deps" | grep -c .)"
bench_sample sim-mcp "packages in Cargo.lock" "$(grep -c '^\[\[package\]\]' Cargo.lock)"
if [ "$BENCH_QUICK" = 1 ]; then
  bench_finish "sim-mcp,rmcp" "--quick: rmcp not downloaded or built."
  exit 0
fi
pkg="$BENCH_WORK/rmcp-scratch"
mkdir -p "$pkg/src"
cp "$BENCH_REPO/rust-toolchain.toml" "$pkg/" 2>/dev/null
printf 'fn main() {}\n' >"$pkg/src/main.rs"
cat >"$pkg/Cargo.toml" <<EOF
[package]
name = "rmcp-scratch"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rmcp = { version = "$rmcp", default-features = false, features = ["server", "transport-io"] }
tokio = { version = "1", default-features = false, features = ["rt", "macros", "io-std"] }
EOF
cd "$pkg" || exit 2
cargo fetch --quiet || bench_die "cargo fetch for rmcp $rmcp failed"
# As published: unique lines of the tree, the scratch package itself included.
bench_sample "rmcp $rmcp" "lines of cargo tree --prefix none -e normal | sort -u" \
  "$(cargo tree -e normal --prefix none | sort -u | wc -l | tr -d ' ')"
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  rm -rf "$pkg/target"
  read -r wall cpu rc <<<"$(CARGO_BUILD_JOBS=2 cpu_timed cargo build --quiet)"
  [ "$rc" = 0 ] || {
    tail -5 "$BENCH_WORK/cpu_timed.log" >&2
    bench_sample "rmcp $rmcp" "clean dev build, wall s" failed
    continue
  }
  bench_sample "rmcp $rmcp" "clean dev build, wall s" "$wall"
  bench_sample "rmcp $rmcp" "clean dev build, CPU s" "$cpu"
done
bench_finish "sim-mcp,rmcp $rmcp" "CARGO_BUILD_JOBS=2; the rmcp package: $pkg/Cargo.toml as above."
