#!/usr/bin/env bash
# Kernel-level shaping (pf + dummynet) for simulator traffic. Needs root: run with sudo.
#
# Targets flows by source port: the simee socket shaper binds shaped sockets to a port range
# when the profile sets `port_base`/`port_count` (simee call network_shape '{"device":"…",
# "inject":true,"port_base":41000,"port_count":1000}'), so only that device's (or app's) traffic
# hits these pipes. Kernel shaping gives real packet loss, jitter-free delay and queueing, for TCP,
# UDP and QUIC alike, which the userspace shaper only approximates.
#
# Rules load into the `com.apple/simee` anchor that macOS's stock /etc/pf.conf already includes
# (`anchor "com.apple/*"` and `dummynet-anchor "com.apple/*"`), so the main ruleset is never
# flushed. pf is enabled with a reference token that `remove` releases.
#
#   sudo scripts/pf-shape.sh apply --ports 41000:41999 --latency-ms 200 --down-kbps 1000 --up-kbps 500 --loss-pct 5
#   sudo scripts/pf-shape.sh status
#   sudo scripts/pf-shape.sh remove
set -euo pipefail
ANCHOR=com.apple/simee
PIPE_OUT=41001
PIPE_IN=41002
STATE=/var/run/simee-pf.token
cmd=${1:-}; shift || true
ports=41000:41999 latency=0 down=0 up=0 loss=0
while [ $# -gt 0 ]; do
  case $1 in
    --ports) ports=$2; shift 2 ;;
    --latency-ms) latency=$2; shift 2 ;;
    --down-kbps) down=$2; shift 2 ;;
    --up-kbps) up=$2; shift 2 ;;
    --loss-pct) loss=$2; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
pipe() { # pipe <id> <kbps>: half the round-trip delay each way
  local args=(delay "$((latency / 2))ms")
  [ "$2" -gt 0 ] && args+=(bw "${2}Kbit/s")
  [ "$loss" -gt 0 ] && args+=(plr "$(awk "BEGIN{print $loss/100}")")
  dnctl pipe "$1" config "${args[@]}"
}
case $cmd in
  apply)
    pipe $PIPE_OUT "$up"
    pipe $PIPE_IN "$down"
    lo=${ports%:*} hi=${ports#*:}
    pfctl -a "$ANCHOR" -f - <<RULES
dummynet out quick proto { tcp udp } from any port $lo:$hi to any pipe $PIPE_OUT
dummynet in quick proto { tcp udp } from any to any port $lo:$hi pipe $PIPE_IN
RULES
    [ -s "$STATE" ] || pfctl -E 2>&1 | awk '/Token/{print $NF}' > "$STATE"
    echo "shaping source ports $ports: latency ${latency}ms down ${down}kbps up ${up}kbps loss ${loss}%"
    ;;
  status)
    pfctl -a "$ANCHOR" -s dummynet 2>/dev/null || true
    dnctl show $PIPE_OUT $PIPE_IN 2>/dev/null || true
    ;;
  remove)
    pfctl -a "$ANCHOR" -F all 2>/dev/null || true
    dnctl pipe delete $PIPE_OUT 2>/dev/null || true
    dnctl pipe delete $PIPE_IN 2>/dev/null || true
    [ -s "$STATE" ] && pfctl -X "$(cat "$STATE")" 2>/dev/null || true
    rm -f "$STATE"
    echo "removed"
    ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
