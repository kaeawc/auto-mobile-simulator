#!/usr/bin/env bash
# Device-free benchmarks: the screenshot path, the frame path, vsock and adb throughput, and the
# renderer's init, on this host with no VM or simulator.
#
#   scripts/bench/host-paths.sh [--only frame|vsock|adb|present|frame-path|renderer] [--lib64 DIR]
#                               [--rounds N] [--quick] [--out DIR]
#
# Each round runs, in release builds of this checkout:
#   frame       cargo run -p simee-core --example frame_bench: pixel conversion, downscales and PNG
#               encoding (the shipping encoder and the alternatives) of docs/desktop-android.png
#               (2480x1740) and a synthetic iOS home screen (1206x2622)
#   vsock       cargo test -p simee-emu-vmm vsock_throughput -- --ignored: the vsock device's guest-to-host
#               and host-to-guest throughput, in process
#   adb         cargo test -p simee-emu-vmm adb_throughput_to_guest -- --ignored: an adb client to the guest
#   present     cargo test -p simee-emu-vmm present_cost -- --ignored: posting a frame into the store at a
#               tablet's and a phone's size
#   frame-path  CASE=2480,1080,true cargo test -p simee-emu frame_path_latency -- --ignored: a post until a
#               display-socket client has the pixels
#   renderer    cargo run -p simee-emu-gfxstream --example renderer_caps -- -emu <lib64> (macOS) or
#               -vulkan-only <lib64> (Linux): the renderer's init as emu runs it, with Metal's shader
#               cache warm after the first round, and a 720x1280 readback; only when a renderer is
#               installed (emu -provision-renderer; --lib64 overrides)
# Defaults: every item, 3 rounds. --quick: one round.
# Reproduces docs/benchmarks.md "Emulator internals" (the screenshot, display stream, vsock, adb
# and renderer-init rows; the published vsock and adb rows were measured on a 4-core Linux VM).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
only=""
lib64=""
while [ $# -gt 0 ]; do
  case $1 in
    --only)
      only=$2
      shift 2
      ;;
    --lib64)
      lib64=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
items=${only:-frame vsock adb present frame-path renderer}
if [ -z "$lib64" ]; then
  if [ "$(uname -s)" = Darwin ]; then sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}; else sdk=${ANDROID_HOME:-$HOME/Android/Sdk}; fi
  lib64=$(find "$sdk/simee/renderer" -maxdepth 2 -type d -name lib64 2>/dev/null | sort | tail -1)
fi
bench_start host-paths
cd "$BENCH_REPO" || exit 2
parse() {
  python3 -I -c '
import re, sys
item, rnd = sys.argv[1], sys.argv[2]
section = ""
def out(arm, metric, v):
    print(f"{rnd}\tthis host\t{arm}: {metric}\t{v}\t")
for line in sys.stdin:
    line = line.rstrip()
    if item == "frame":
        m = re.match(r"^(.+): (\d+x\d+)$", line)
        if m:
            section = m.group(2); continue
        m = re.match(r"^(\S.*?)\s{2,}([0-9.]+) ms\s+\((\d+) bytes\)", line)
        if m and section:
            out(f"frame_bench {section}", m.group(1) + ", ms", m.group(2))
    elif item in ("vsock", "adb"):
        m = re.search(r"(vsock|adb) (\S+): .* = ([0-9.]+) MiB/s", line)
        if m:
            out(m.group(1), m.group(2) + ", MiB/s", m.group(3))
    elif item == "present":
        m = re.search(r"(\d+x\d+) frame into the store: ([0-9.]+) ms \(flip, then copy\) -> ([0-9.]+) ms \(copy\)", line)
        if m:
            out("present_cost", m.group(1) + ": flip then copy, ms", m.group(2))
            out("present_cost", m.group(1) + ": copy, ms", m.group(3))
    elif item == "frame-path":
        m = re.search(r"(\d+x\d+ \S+ \d+ \S+)\s*: post -> client has pixels ([0-9.]+) ms", line)
        if m:
            out("frame_path_latency", m.group(1) + ": post to client has pixels, ms", m.group(2))
    elif item == "renderer":
        m = re.match(r"^initialized in ([0-9.]+) ms", line)
        if m:
            out("renderer_caps", "renderer init, ms", m.group(1))
        m = re.match(r"^readback (\d+x\d+): matches \(median ([0-9.]+) ms", line)
        if m:
            out("renderer_caps", m.group(1) + " readback, ms", m.group(2))
' "$1" "$BENCH_ROUND"
}
run() {
  case $1 in
    frame) cargo run -q --release -p simee-core --example frame_bench ;;
    vsock) cargo test -q --release -p simee-emu-vmm vsock_throughput -- --ignored --nocapture --test-threads=1 ;;
    adb) cargo test -q --release -p simee-emu-vmm adb_throughput_to_guest -- --ignored --nocapture --test-threads=1 ;;
    present) cargo test -q --release -p simee-emu-vmm present_cost -- --ignored --nocapture --test-threads=1 ;;
    frame-path) CASE=2480,1080,true cargo test -q --release -p simee-emu frame_path_latency -- --ignored --nocapture --test-threads=1 ;;
    renderer)
      if [ -z "$lib64" ] || [ ! -d "$lib64" ]; then
        bench_log "renderer: no lib64 (emu -provision-renderer); skipped"
        return 0
      fi
      if [ "$(uname -s)" = Darwin ]; then
        cargo run -q --release -p simee-emu-gfxstream --example renderer_caps -- -emu "$lib64"
      else
        cargo run -q --release -p simee-emu-gfxstream --example renderer_caps -- -vulkan-only "$lib64"
      fi
      ;;
    *) bench_die "unknown item $1" ;;
  esac
}
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  for item in $items; do
    if run "$item" >"$BENCH_WORK/$item.out" 2>&1; then
      parse "$item" <"$BENCH_WORK/$item.out" >>"$BENCH_SAMPLES"
    else
      tail -5 "$BENCH_WORK/$item.out" >&2
      bench_sample "$item" "run" failed
    fi
    bench_log "round $BENCH_ROUND $item done"
  done
done
bench_finish "" "Release builds of this checkout; no device. Renderer lib64: ${lib64:-none}."
