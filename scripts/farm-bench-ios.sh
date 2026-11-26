#!/usr/bin/env bash
# Farm benchmark for iOS simulators: boot N devices at once through simee and measure how long
# until all of them are settled, what they cost the host, and what they cost once idle.
#
#   scripts/farm-bench-ios.sh N [N...]   -> one key=value line per N and round (also bench/farm-ios-<label>-<N>-r<round>.result)
#
# Every device is created for the run (named $PREFIX<label>-<i>), booted, measured, shut down and
# deleted (KEEP=1 keeps them). Settled is judged here, from outside, the same way for every mode:
# each device's launchd_sim process tree uses under SETTLE_CPU % of one core over SETTLE_WINDOW
# seconds (CPU from each process's accumulated time in ps, so a process that exits doesn't make
# the tree look quieter). A simee `boot` that returns early or late doesn't change it.
#
# Environment (defaults in brackets):
#   SIMEE        the simee binary [target/release/simee]
#   LABEL         run label [farm]
#   PREFIX        device name prefix [perffarm-]
#   DEVICE_TYPE   [iPhone 17]   RUNTIME [com.apple.CoreSimulator.SimRuntime.iOS-27-2]
#   MODE          fresh (create) | clone (`simee clone TEMPLATE`, a shut-down device) [fresh]
#   TEMPLATE      the device to clone in MODE=clone
#   BOOT          each (one `simee boot` per device, all at once) | many (`simee boot-many`) [each]
#   K             boot-many's concurrency [unset: its default]
#   BOOT_ARGS     extra arguments for `simee boot` / `boot-many` (e.g. --settle, --profile slim)
#   ROUNDS        boot the same devices this many times (round 1 is their first boot) [1]
#   SETTLE_CPU    [10]  SETTLE_WINDOW [10]  TIMEOUT [600] seconds  IDLE_AFTER [60] seconds
#   ON_CREATE     a command run with each new device's UDID as its last argument (e.g. to record it)
#   KEEP=1        don't delete the devices (they are still shut down)
#
# Memory: `mem_mb_per` is the system-wide delta (vm_stat: anonymous + wired + compressor-occupied
# pages) from before the boots to settle, divided by N: other work on the host moves it, so read
# it over several runs. `fp_mb_per` is `footprint` over every process of the devices at idle
# (dirty memory, shared pages counted once), divided by N: per-process RSS double-counts the
# runtime's shared libraries hundreds of times.
set -uo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
SIMEE=${SIMEE:-$repo/target/release/simee}
LABEL=${LABEL:-farm}; PREFIX=${PREFIX:-perffarm-}
DEVICE_TYPE=${DEVICE_TYPE:-iPhone 17}; RUNTIME=${RUNTIME:-com.apple.CoreSimulator.SimRuntime.iOS-27-2}
MODE=${MODE:-fresh}; BOOT=${BOOT:-each}; BOOT_ARGS=${BOOT_ARGS:-}; ROUNDS=${ROUNDS:-1}
SETTLE_CPU=${SETTLE_CPU:-10}; SETTLE_WINDOW=${SETTLE_WINDOW:-10}; TIMEOUT=${TIMEOUT:-600}; IDLE_AFTER=${IDLE_AFTER:-60}
[ $# -ge 1 ] || { sed -n '2,6p' "$0" >&2; exit 2; }
[ -x "$SIMEE" ] || { echo "no simee at $SIMEE (cargo build --release -p simee)" >&2; exit 2; }
out="$repo/bench"; mkdir -p "$out"
work=$(mktemp -d "${TMPDIR:-/tmp}/farm-bench.XXXXXX")

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { awk -v a="$(now)" -v b="$1" 'BEGIN { printf "%.1f\n", a - b }'; }
# System memory in use, MB: anonymous + wired + compressor-occupied pages.
mem_mb() {
  vm_stat | awk '/page size of/ { ps = $8 } /^Anonymous pages/ { a = $3 } /^Pages wired down/ { w = $4 }
    /^Pages occupied by compressor/ { c = $5 } END { printf "%d\n", (a + w + c) * ps / 1048576 }'
}
# A ps snapshot (pid ppid time args) into file $1.
snap() { ps -Ao pid=,ppid=,time=,args= > "$1"; }
# Per device in udids (from snapshot $2, against the earlier snapshot $1): UDID, CPU seconds its
# tree used in between (each process's increase; a new process counts all its time), process
# count, and the tree's pids (comma-separated). A device with no launchd_sim yet: UDID 0 0 -.
trees() {
  awk -v list="${udids[*]}" '
    function secs(t,   n, p, s, i, d) { d = 0; if (index(t, "-")) { split(t, p, "-"); d = p[1]; t = p[2] }
      n = split(t, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; return d * 86400 + s }
    BEGIN { nu = split(list, udids, " ") }
    FNR == NR { before[$1] = secs($3); next }
    { pid = $1; ppid[pid] = $2; cpu[pid] = secs($3); pids[++np] = pid
      if ($4 == "launchd_sim" || $4 ~ /\/launchd_sim$/) for (u = 1; u <= nu; u++) if (index($0, "/" udids[u] "/")) root[udids[u]] = pid }
    END {
      for (u = 1; u <= nu; u++) {
        d = udids[u]
        if (!(d in root)) { print d, 0, 0, "-"; continue }
        delete in_tree; in_tree[root[d]] = 1; changed = 1
        while (changed) { changed = 0; for (i = 1; i <= np; i++) { p = pids[i]; if (!(p in in_tree) && (ppid[p] in in_tree)) { in_tree[p] = 1; changed = 1 } } }
        c = 0; n = 0; pl = ""
        for (p in in_tree) { used = cpu[p] - ((p in before) ? before[p] : 0); if (used > 0) c += used; n++; pl = pl (pl == "" ? "" : ",") p }
        printf "%s %.2f %d %s\n", d, c, n, pl
      }
    }' "$1" "$2"
}
# Sum over devices of tree CPU % (of one core) over the next $1 seconds.
cpu_over() {
  snap "$work/a"; sleep "$1"; snap "$work/b"
  trees "$work/a" "$work/b" | awk -v s="$1" '{ t += $2 } END { printf "%.0f\n", 100 * t / s }'
}
# footprint of every process of the devices, MB (shared pages once).
footprint_mb() {
  snap "$work/a"; snap "$work/b"
  local p; p=$(trees "$work/a" "$work/b" | awk '$4 != "-" { gsub(",", " ", $4); printf "%s ", $4 }')
  [ -n "$p" ] || { echo 0; return; }
  # shellcheck disable=SC2086
  footprint --noCategories $p 2>/dev/null | awk '/Summary Footprint:/ { v = $3; if ($4 == "GB") v *= 1024; if ($4 == "KB") v /= 1024; printf "%d\n", v }'
}
shutdown_all() {
  for u in "${udids[@]}"; do
    "$SIMEE" shutdown "$u" 2>&1 | grep -v 'current state: Shutdown' | tail -1
  done
  for u in "${udids[@]}"; do
    for _ in $(seq 1 30); do
      "$SIMEE" list --platform ios | grep -qF "($u) (Shutdown)" && break
      sleep 1
    done
  done
}
cleanup_devices() {
  shutdown_all
  [ "${KEEP:-0}" = 1 ] && return
  for u in "${udids[@]}"; do
    if "$SIMEE" delete "$u"; then echo "deleted $u"; else echo "delete $u FAILED"; fi
  done
}

# Boots every device in udids, waits until all are settled (or TIMEOUT), measures; prints the line.
measure() {
  local n=$1 round=$2 t0 p pids=() fails=0 booted_s="" t cur prev=$work/prev mem0 mem1 mem2
  rm -f "$work"/hist-* "$work"/settled-* "$work/trace"
  mem0=$(mem_mb)
  snap "$prev"; t0=$(now)
  if [ "$BOOT" = many ]; then
    # shellcheck disable=SC2086
    "$SIMEE" boot-many "${udids[@]}" ${K:+-j "$K"} --timeout "$TIMEOUT" $BOOT_ARGS > "$work/many.out" 2>&1 &
    pids+=($!)
  else
    for u in "${udids[@]}"; do
      # shellcheck disable=SC2086
      "$SIMEE" boot "$u" --timeout "$TIMEOUT" $BOOT_ARGS > "$work/boot-$u.out" 2>&1 &
      pids+=($!)
    done
  fi
  # Settle watch, once a second. Per device (bash 3.2 has no associative arrays):
  # $work/hist-<udid> (recent dt:used intervals) and $work/settled-<udid> (its settle time).
  local deadline tp=$t0
  deadline=$(awk -v a="$t0" -v t="$TIMEOUT" 'BEGIN { printf "%.3f\n", a + t }')
  while :; do
    sleep 1
    cur=$work/cur; snap "$cur"; t=$(now)
    if [ -z "$booted_s" ]; then
      alive=0; for p in "${pids[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
      [ $alive = 0 ] && booted_s=$(since "$t0")
    fi
    while read -r u used nproc _; do
      [ -f "$work/settled-$u" ] && continue
      [ "$nproc" -gt 0 ] || continue
      pct=$(awk -v c="$used" -v a="$t" -v b="$tp" 'BEGIN { printf "%d\n", 100 * c / (a - b) }')
      # The window is in seconds, not samples: under load one pass of this loop can take many
      # seconds. Entries are dt:used; the oldest goes once the rest still span the window.
      h=$(echo "$(cat "$work/hist-$u" 2>/dev/null) $(awk -v a="$t" -v b="$tp" -v c="$used" 'BEGIN { printf "%.3f:%.3f", a - b, c }')" | awk -v w="$SETTLE_WINDOW" '
        { n = NF; span = 0; for (i = 1; i <= n; i++) { split($i, e, ":"); dt[i] = e[1]; span += e[1] }
          s = 1; while (s < n && span - dt[s] >= w) { span -= dt[s]; s++ }
          o = ""; for (i = s; i <= n; i++) o = o " " $i; print o }')
      echo "$h" > "$work/hist-$u"
      echo "$u $(awk -v a="$t" -v b="$t0" 'BEGIN { printf "%.1f", a - b }') $pct $nproc" >> "$work/trace"
      if echo "$h" | awk -v m="$SETTLE_CPU" -v w="$SETTLE_WINDOW" '{ span = 0; used = 0; for (i = 1; i <= NF; i++) { split($i, e, ":"); span += e[1]; used += e[2] }
           exit !(span >= w && 100 * used / span < m) }'; then
        # When it was known to be settled: the end of its first quiet window (as `boot --settle --settle-cpu all`: this script judges all CPU, `boot --settle` now only foreground).
        since "$t0" > "$work/settled-$u"
      fi
    done < <(trees "$prev" "$cur")
    mv "$cur" "$prev"; tp=$t
    [ "$(find "$work" -name 'settled-*' | wc -l | tr -d ' ')" -ge "${#udids[@]}" ] && break
    awk -v a="$t" -v d="$deadline" 'BEGIN { exit !(a > d) }' && break
  done
  local all_s; all_s=$(since "$t0")
  for p in "${pids[@]}"; do wait "$p" || fails=$((fails + 1)); done
  [ -z "$booted_s" ] && booted_s=$(since "$t0")
  local unsettled=$(( ${#udids[@]} - $(find "$work" -name 'settled-*' | wc -l) ))
  mem1=$(mem_mb)
  local procs cpu_settle cpu_idle fp st
  snap "$work/a"; procs=$(trees "$work/a" "$work/a" | awk '{ n += $3; c++ } END { if (c) printf "%d\n", n / c; else print 0 }')
  cpu_settle=$(cpu_over 5)
  sleep "$IDLE_AFTER"
  cpu_idle=$(cpu_over 5)
  mem2=$(mem_mb)
  fp=$(footprint_mb)
  # Per-device settle times (seconds from the start of the boots), sorted.
  st=$(cat "$work"/settled-* 2>/dev/null | sort -n | paste -sd, -)
  local c=${#udids[@]}
  line="label=$LABEL mode=$MODE boot=$BOOT k=${K:-} args=\"$BOOT_ARGS\" n=$c round=$round create_s=$create_s data_mb_per=$(( c ? data_mb / c : 0 )) disk_mb_per=$(( c ? disk_mb / c : 0 ))"
  line+=" boot_returned_s=$booted_s all_settled_s=$all_s settle_s=[$st] unsettled=$unsettled boot_failures=$fails"
  line+=" mem_mb_per=$(( c ? (mem1 - mem0) / c : 0 )) mem_mb_per_idle=$(( c ? (mem2 - mem0) / c : 0 )) fp_mb_per=$(( c ? ${fp:-0} / c : 0 ))"
  line+=" procs_per=$procs cpu_pct_settle=$cpu_settle cpu_pct_idle${IDLE_AFTER}s=$cpu_idle"
  echo "$line" | tee "$out/farm-ios-$LABEL-$n-r$round.result"
  cp "$work/trace" "$out/farm-ios-$LABEL-$n-r$round.trace" 2>/dev/null
  [ "$BOOT" = many ] && cp "$work/many.out" "$out/farm-ios-$LABEL-$n-r$round.boot-many" 2>/dev/null
  return 0
}

for n in "$@"; do
  udids=()
  disk0=$(df -m "$HOME" | awk 'NR == 2 { print $3 }')
  t_create=$(now)
  for i in $(seq 1 "$n"); do
    name="$PREFIX$LABEL-$i"
    case "$MODE" in
      fresh) u=$("$SIMEE" create "$name" "$DEVICE_TYPE" "$RUNTIME" 2>&1 | tail -1) ;;
      clone) u=$("$SIMEE" clone "${TEMPLATE:?MODE=clone needs TEMPLATE}" "$name" 2>&1 | tail -1) ;;
      *) echo "MODE must be fresh or clone" >&2; exit 2 ;;
    esac
    [[ "$u" =~ ^[0-9A-F-]{36}$ ]] || { echo "create $name failed: $u" >&2; continue; }
    udids+=("$u"); echo "created $name $u" >&2
    if [ -n "${ON_CREATE:-}" ]; then $ON_CREATE "$u" >&2; fi
  done
  create_s=$(since "$t_create")
  # Disk: `du` of each device's directory (an APFS clone's shared blocks count in full) and the
  # volume's used space before and after creating them (what the copies really took).
  disk_mb=$(( $(df -m "$HOME" | awk 'NR == 2 { print $3 }') - disk0 ))
  data_mb=0
  for u in "${udids[@]}"; do data_mb=$((data_mb + $(du -sm "$HOME/Library/Developer/CoreSimulator/Devices/$u" | cut -f1))); done
  for round in $(seq 1 "$ROUNDS"); do
    measure "$n" "$round"
    [ "$round" -lt "$ROUNDS" ] && shutdown_all >&2
  done
  cleanup_devices >&2
done
