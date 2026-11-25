#!/usr/bin/env bash
# Frame pacing of a booted Android device under steady UI motion: 10 app-drawer swipes up and
# down on the launcher, then the launcher's own frame stats (`dumpsys gfxinfo`), and the host CPU
# of the emulator process over the run when its PID is given. The same on emu and the stock
# emulator, so results compare. Run it on a settled device (a minute after boot).
#
#   scripts/android-jank.sh <serial> [emulator pid]   -> one key=value line
#
# janky_pct is gfxinfo's "Janky frames" (frames that missed their deadline); p50/p90/p99 are its
# frame-time percentiles in ms; host_cpu_pct is the PID's CPU time over the run's wall time.
set -uo pipefail
serial=${1:?usage: scripts/android-jank.sh <serial> [emulator pid]}
pid=${2:-}
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
adb() { "$sdk/platform-tools/adb" -s "$serial" "$@"; }
pkg=$(adb shell cmd package resolve-activity --brief -c android.intent.category.HOME -a android.intent.action.MAIN | tail -1 | tr -d '\r' | cut -d/ -f1)
read -r w h < <(adb shell wm size | tail -1 | tr -d '\r' | sed 's/.*: //; s/x/ /')
cpu() { [ -n "$pid" ] && ps -o time= -p "$pid" | awk -F: '{ if (NF == 3) print $1*3600 + $2*60 + $3; else print $1*60 + $2 }'; }
adb shell input keyevent KEYCODE_HOME
sleep 2
adb shell dumpsys gfxinfo "$pkg" reset > /dev/null
c0=$(cpu); t0=$(date +%s)
x=$((w / 2)); low=$((h * 85 / 100)); high=$((h * 25 / 100))
for _ in 1 2 3 4 5 6 7 8 9 10; do
  adb shell input swipe $x $low $x $high 250; sleep 1
  adb shell input swipe $x $high $x $low 250; sleep 1
done
c1=$(cpu); t1=$(date +%s)
stats=$(adb shell dumpsys gfxinfo "$pkg")
field() { printf '%s\n' "$stats" | grep -m1 "$1" | sed -E "s/.*$1:? *//; s/ms.*//; s/ .*//"; }
janky=$(printf '%s\n' "$stats" | grep -m1 'Janky frames:' | sed -E 's/.*\(([0-9.]+)%\).*/\1/')
cpu_pct=$( [ -n "$pid" ] && awk -v a="$c0" -v b="$c1" -v t=$((t1 - t0)) 'BEGIN { printf "%.0f", (b - a) * 100 / t }')
echo "screen=${w}x${h} frames=$(field 'Total frames rendered') janky_pct=$janky p50=$(field '50th percentile') p90=$(field '90th percentile') p99=$(field '99th percentile') host_cpu_pct=${cpu_pct:-}"
