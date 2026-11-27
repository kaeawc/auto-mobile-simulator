#!/usr/bin/env bash
# Golden user data benchmark (#226): makes one AVD of an image the way `emu -create-avd` does,
# boots it, and prints one key=value line: how long the create took, and the time from starting
# emu to `sys.boot_completed` and to the launcher being the resumed activity. The AVD is deleted
# afterwards.
#
#   scripts/golden-bench.sh golden <name> <sysdir>     create with the image's golden (the first
#                                                      AVD of an image builds it: its `create_s`
#                                                      is the one-time cost, and it's not a "second AVD")
#   scripts/golden-bench.sh fresh  <name> <sysdir>     create with -no-golden: a first boot
#
# <sysdir> is relative to the SDK, e.g. system-images/android-36/default/x86_64. Environment:
#   EMU          the emu binary (default: this checkout's target/release/emu)
#   EMU_ARGS     more `-create-avd` flags (default: -memory 1536 -cores 4)
#   ANDROID_HOME, ANDROID_AVD_HOME, EMU_QEMU as for emu; adb is $ANDROID_HOME/platform-tools/adb
#   BOOT_TIMEOUT seconds to wait for boot and for the launcher (default 1500)
#
# A median of three: run it three times with new names, e.g. for N in 1 2 3.
set -euo pipefail

mode=${1:?golden|fresh}
name=${2:?AVD name}
sysdir=${3:?system image dir}
root=$(cd "$(dirname "$0")/.." && pwd)
emu=${EMU:-$root/target/release/emu}
adb=${ANDROID_HOME:?set ANDROID_HOME}/platform-tools/adb
timeout_s=${BOOT_TIMEOUT:-1500}
extra=${EMU_ARGS:--memory 1536 -cores 4}
out=$(mktemp -d)
now() { date +%s.%N; }
since() { awk -v a="$(now)" -v b="$1" 'BEGIN {printf "%.1f", a-b}'; }

flags=()
[ "$mode" = fresh ] && flags=(-no-golden)

t0=$(now)
# shellcheck disable=SC2086
"$emu" -create-avd "$name" -sysdir "$sysdir" $extra ${flags[@]+"${flags[@]}"} >"$out/create.log" 2>&1
create_s=$(since "$t0")

t1=$(now)
"$emu" -avd "$name" -no-window -no-audio >"$out/boot.log" 2>&1 &
pid=$!
trap 'kill -INT "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true' EXIT

port=
boot_s=
launcher_s=
while awk -v t="$(since "$t1")" -v m="$timeout_s" 'BEGIN {exit !(t < m)}'; do
  kill -0 "$pid" 2>/dev/null || {
    echo "emu exited early: $(tail -3 "$out/boot.log")" >&2
    break
  }
  [ -n "$port" ] || port=$(sed -n 's/^emu: .* on emulator-\([0-9]*\) .*/\1/p' "$out/boot.log" | head -1)
  if [ -z "$boot_s" ] && grep -q '^emu: boot completed' "$out/boot.log"; then boot_s=$(since "$t1"); fi
  if [ -n "$boot_s" ] && [ -n "$port" ]; then
    if "$adb" -s "emulator-$port" shell dumpsys activity activities 2>/dev/null | grep -i ResumedActivity | grep -qi launcher; then
      launcher_s=$(since "$t1")
      break
    fi
  fi
  sleep 2
done

# The console's stop, as emu's golden build does it.
[ -n "$port" ] && "$adb" -s "emulator-$port" emu kill >/dev/null 2>&1 || true
wait "$pid" 2>/dev/null || true
trap - EXIT
"$emu" -delete-avd "$name" >/dev/null
echo "mode=$mode create_s=$create_s boot_s=${boot_s:-timeout} launcher_s=${launcher_s:-timeout} avd=$name"
sed 's/^/# /' "$out/create.log"
rm -rf "$out"
