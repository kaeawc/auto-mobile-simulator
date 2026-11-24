#!/usr/bin/env bash
# Device-farm benchmark: boots N simee devices at once and measures time until all are settled,
# what they cost the host while idle, and how many failed. One key=value line per run, also
# appended to <out>/farm.results.
#
#   scripts/farm-bench-android.sh <label> <golden-avd> <N> [emu args...]
#
# Devices are AVDs named perffarm-1..N, made for the run and deleted after it (KEEP=1 leaves them):
#   MODE=clone (default)  `emu -clone-avd perffarm-<i> -from <golden>`: APFS clones of the golden
#                         AVD's settled simee disks, so no device pays first boot
#   MODE=fresh            `emu -create-avd perffarm-<i> -sysdir <golden's image>`: a first boot each
#   MODE=existing         boot the AVDs named in AVDS (space-separated) as they are; never deleted
# Other knobs: K (devices started at once, default N; the next starts when one settles), BASE_PORT
# (default 5600; device i gets console BASE_PORT+2(i-1), adb +1), EMU (the emu binary, default
# target/release/emu), OUT (default bench/), BOOT_TIMEOUT (s from each device's own launch, to
# settled, default 300; boot and settle times are from its launch too), SETTLE_CPU (% of one
# core, default 60), SETTLE_S (default 5), IDLE_S (default 10), AFTER_S (default 60).
#
# Settled: the guest says sys.boot_completed=1, and the emu process then used under SETTLE_CPU %
# of one host core over SETTLE_S seconds (host-side: no adb load on a busy guest). That is where
# a farm can hand the device to a test without it fighting its own boot work.
#
# Memory is reported two ways because per-process RSS counts Hypervisor.framework-mapped guest
# pages that host and guest both touched twice: rss_mb (sum of the emu processes' RSS) and sys_mb
# (system-wide change in anonymous + wired + compressor pages from vm_stat since before launch).
# Both at settle and AFTER_S seconds later. Other load on the host moves sys_mb: read it with
# the host load the line records.
#
# Per device at idle (after AFTER_S): host CPU % (from the process's CPU time over IDLE_S) and vCPU
# exits/s (from EMU_EXIT_PROFILE, which every device is started with), plus the PL011's share.
#
# Stops only the PIDs it started (kill -INT, then waits), never touches other emulators.
set -uo pipefail
label=${1:?usage: scripts/farm-bench-android.sh <label> <golden-avd> <N> [emu args...]}
golden=${2:?golden avd}
n=${3:?N}
shift 3
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
adb="$sdk/platform-tools/adb"
emu=${EMU:-$repo/target/release/emu}
# Makes and deletes the AVDs (-clone-avd, -create-avd, -delete-avd): this checkout's emu, so an
# older EMU (a baseline build) can be benchmarked on the same clones.
tool=${EMU_TOOL:-$repo/target/release/emu}
mode=${MODE:-clone}
k=${K:-$n}
base_port=${BASE_PORT:-5600}
out=${OUT:-$repo/bench}
run="$out/farm-$label"
mkdir -p "$run"
avd_home=${ANDROID_AVD_HOME:-$HOME/.android/avd}
boot_timeout=${BOOT_TIMEOUT:-300}
settle_cpu=${SETTLE_CPU:-60}
settle_s=${SETTLE_S:-5}
idle_s=${IDLE_S:-10}
after_s=${AFTER_S:-60}

case "$base_port" in *[!0-9]*) echo "BASE_PORT must be a number" >&2; exit 2 ;; esac
if [ "$base_port" -lt 5600 ] && [ -z "${ALLOW_LOW_PORTS:-}" ]; then
  echo "BASE_PORT $base_port is below 5600 (ports 5554-5598 belong to other emulators here)" >&2
  exit 2
fi
codesign -s - -f --entitlements "$repo/scripts/hypervisor.entitlements" "$emu" 2>&1 | grep -v 'replacing existing signature' || true

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
# CPU seconds a process has used (ps prints [[dd-]hh:]mm:ss.cc).
cpu_s() { ps -o time= -p "$1" 2>/dev/null | awk '{n=split($1,a,/[-:]/); s=0; for(i=1;i<=n;i++) s=s*60+a[i]; print s}'; }
# System-wide anonymous + wired + compressor-occupied memory, MB.
sys_mb() {
  vm_stat | awk '/page size of/ {ps=$8} /^Anonymous pages/ {a=$3} /^Pages wired down/ {w=$4} /^Pages occupied by compressor/ {c=$5}
    END {printf "%.0f\n", (a+w+c)*ps/1048576}'
}
free_mb() { vm_stat | awk '/page size of/ {ps=$8} /^Pages free/ {f=$3} /^Pages speculative/ {s=$3} END {printf "%.0f\n", (f+s)*ps/1048576}'; }
rss_mb() { ps -o rss= -p "$1" 2>/dev/null | awk '{s+=$1} END {printf "%.0f\n", s/1024}'; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
# Sum of vCPU runs (= exits) and of PL011 accesses in an EMU_EXIT_PROFILE snapshot.
exits() { awk '$1=="C" {c+=$3} $1=="R" && $2=="pl011" {p+=$4+$5} END {printf "%d %d\n", c, p}' "$1" 2>/dev/null; }

names=()
case "$mode" in
  existing) read -r -a names <<< "${AVDS:?MODE=existing needs AVDS}" ;;
  clone | fresh) for i in $(seq 1 "$n"); do names+=("perffarm-$i"); done ;;
  *) echo "MODE must be clone, fresh or existing" >&2; exit 2 ;;
esac
n=${#names[@]}

# Make the devices (timed: what a farm pays before booting).
t_make0=$(now)
if [ "$mode" != existing ]; then
  sysdir=$(sed -n 's/^image.sysdir.1 *= *//p' "$avd_home/$golden.avd/config.ini" | head -1)
  df0=$(df -k "$avd_home" | awk 'NR==2 {print $3}')
  for name in "${names[@]}"; do
    if [ -e "$avd_home/$name.avd" ]; then echo "$name already exists; delete it first" >&2; exit 2; fi
    if [ "$mode" = clone ]; then
      "$tool" -clone-avd "$name" -from "$golden" > "$run/make-$name.log" 2>&1 || { cat "$run/make-$name.log" >&2; exit 1; }
    else
      "$tool" -create-avd "$name" -sysdir "$sysdir" > "$run/make-$name.log" 2>&1 || { cat "$run/make-$name.log" >&2; exit 1; }
    fi
  done
  df1=$(df -k "$avd_home" | awk 'NR==2 {print $3}')
  disk_df_mb=$(( (df1 - df0) / 1024 ))
  disk_du_mb=$(for name in "${names[@]}"; do du -sk "$avd_home/$name.avd"; done | awk '{s+=$1} END {printf "%.0f", s/1024}')
fi
t_make=$(awk -v a="$(now)" -v b="$t_make0" 'BEGIN {printf "%.2f", a-b}')

sys0=$(sys_mb); free0=$(free_mb); load0=$(load1)
pids=(); ports=(); launched_at=(); deadline=(); booted_at=(); settled_at=(); state=(); last_cpu=(); last_t=()
started=0
t0=$(now)
launch() {
  local i=$1 name=${names[$1]} port=$((base_port + 2 * $1))
  shift
  env -i HOME="$HOME" USER="$USER" TMPDIR="$TMPDIR" PATH="/usr/bin:/bin:/usr/sbin:/sbin" ANDROID_HOME="$sdk" ANDROID_SDK_ROOT="$sdk" \
    EMU_EXIT_PROFILE="$run/exits-$name.txt" ${EMU_CONSOLE:+EMU_CONSOLE="$EMU_CONSOLE"} \
    "$emu" -avd "$name" -port "$port" -no-window -no-audio -no-boot-anim -no-snapshot-save "$@" > "$run/emu-$name.log" 2>&1 &
  pids[i]=$!; ports[i]=$port; state[i]=booting; booted_at[i]=""; settled_at[i]=""
  # Each device gets BOOT_TIMEOUT from its own launch: with K < N, later ones start later.
  launched_at[i]=$(now)
  deadline[i]=$(awk -v a="${launched_at[i]}" -v b="$boot_timeout" 'BEGIN {print a+b}')
  echo "$name pid=${pids[i]} port=$port" >> "$run/pids"
  started=$((started + 1))
}
stop_all() {
  for i in "${!pids[@]}"; do kill -INT "${pids[i]}" 2>/dev/null; done
  for i in "${!pids[@]}"; do
    for _ in $(seq 1 100); do kill -0 "${pids[i]}" 2>/dev/null || break; sleep 0.1; done
  done
}
trap 'stop_all' EXIT INT TERM

while [ "$started" -lt "$n" ] && [ "$started" -lt "$k" ]; do launch "$started" "$@"; done
failures=0
while :; do
  active=0
  for i in $(seq 0 $((n - 1))); do
    [ "$i" -lt "$started" ] || continue
    case "${state[i]}" in settled | failed) continue ;; esac
    active=$((active + 1))
    pid=${pids[i]}
    if ! kill -0 "$pid" 2>/dev/null; then state[i]=failed; failures=$((failures + 1)); active=$((active - 1)); continue; fi
    t=$(now)
    if awk -v a="$t" -v b="${deadline[i]}" 'BEGIN {exit !(a>b)}'; then
      state[i]=failed; failures=$((failures + 1)); active=$((active - 1)); kill -INT "$pid" 2>/dev/null; continue
    fi
    if [ "${state[i]}" = booting ]; then
      if grep -q '^emu: boot completed' "$run/emu-${names[i]}.log" 2>/dev/null &&
        [ "$("$adb" -s "emulator-${ports[i]}" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; then
        state[i]=booted; booted_at[i]=$t; last_cpu[i]=$(cpu_s "$pid"); last_t[i]=$t
      fi
    elif awk -v a="$t" -v b="${last_t[i]}" -v s="$settle_s" 'BEGIN {exit !(a-b>=s)}'; then
      c=$(cpu_s "$pid")
      pct=$(awk -v c="$c" -v p="${last_cpu[i]}" -v a="$t" -v b="${last_t[i]}" 'BEGIN {printf "%.0f", 100*(c-p)/(a-b)}')
      if [ "$pct" -lt "$settle_cpu" ]; then
        state[i]=settled; settled_at[i]=$t; active=$((active - 1))
      else
        last_cpu[i]=$c; last_t[i]=$t
      fi
    fi
  done
  # Admission: at most K devices booting or settling at once.
  while [ "$active" -lt "$k" ] && [ "$started" -lt "$n" ]; do launch "$started" "$@"; active=$((active + 1)); done
  [ "$active" -eq 0 ] && break
  sleep 0.25
done
t_all=$(awk -v a="$(now)" -v b="$t0" 'BEGIN {printf "%.1f", a-b}')
sys1=$(sys_mb); rss1=0
for i in "${!pids[@]}"; do r=$(rss_mb "${pids[i]}"); rss1=$((rss1 + ${r:-0})); done
boots=""; settles=""
for i in $(seq 0 $((n - 1))); do
  # From the device's own launch, not the run's start (the same when K >= N).
  [ -n "${booted_at[i]:-}" ] && boots+=" $(awk -v a="${booted_at[i]}" -v b="${launched_at[i]}" 'BEGIN {printf "%.1f", a-b}')"
  [ -n "${settled_at[i]:-}" ] && settles+=" $(awk -v a="${settled_at[i]}" -v b="${launched_at[i]}" 'BEGIN {printf "%.1f", a-b}')"
done
med() { tr ' ' '\n' | grep . | sort -n | awk '{a[NR]=$1} END {if (NR) print a[int((NR+1)/2)]; else print "-"}'; }

sleep "$after_s"
sys2=$(sys_mb); rss2=0
for i in "${!pids[@]}"; do r=$(rss_mb "${pids[i]}"); rss2=$((rss2 + ${r:-0})); done
# Idle cost per device, measured over the same IDLE_S window for all.
declare -a c0 e0 p0
ta=$(now)
for i in "${!pids[@]}"; do
  c0[i]=$(cpu_s "${pids[i]}"); read -r "e0[i]" "p0[i]" <<< "$(exits "$run/exits-${names[i]}.txt")"
done
sleep "$idle_s"
tb=$(now)
cpus=""; rates=""; shares=""
for i in "${!pids[@]}"; do
  [ "${state[i]}" = settled ] || continue
  c1=$(cpu_s "${pids[i]}"); read -r e1 p1 <<< "$(exits "$run/exits-${names[i]}.txt")"
  cpus+=" $(awk -v c="$c1" -v p="${c0[i]}" -v a="$tb" -v b="$ta" 'BEGIN {printf "%.1f", 100*(c-p)/(a-b)}')"
  rates+=" $(awk -v x="$e1" -v y="${e0[i]:-0}" -v a="$tb" -v b="$ta" 'BEGIN {printf "%.0f", (x-y)/(a-b)}')"
  shares+=" $(awk -v x="$p1" -v y="${p0[i]:-0}" -v e="$e1" -v f="${e0[i]:-0}" 'BEGIN {d=e-f; s=0; if (d>0) s=100*(x-y)/d; printf "%.0f", s}')"
done
stop_all
trap - EXIT INT TERM

line="label=$label mode=$mode n=$n k=$k make_s=$t_make all_settled_s=$t_all"
line+=" boot_med_s=$(echo "$boots" | med) settle_med_s=$(echo "$settles" | med) failures=$failures"
line+=" rss_mb=$rss1 sys_mb=$((sys1 - sys0)) rss_mb_after=$rss2 sys_mb_after=$((sys2 - sys0)) free_mb_start=$free0"
line+=" idle_cpu_pct_med=$(echo "$cpus" | med) idle_cpu_pct=[${cpus# }] idle_exits_s_med=$(echo "$rates" | med) pl011_pct_med=$(echo "$shares" | med)"
[ -n "${disk_df_mb:-}" ] && line+=" disk_df_mb=$disk_df_mb disk_du_mb=$disk_du_mb"
line+=" load_start=$load0 load_end=$(load1)"
echo "$line" | tee -a "$out/farm.results"

if [ "$mode" != existing ] && [ -z "${KEEP:-}" ]; then
  for name in "${names[@]}"; do "$tool" -delete-avd "$name" 2>&1 | tail -1; done
fi
