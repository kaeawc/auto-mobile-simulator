#!/bin/sh
# cargo runner: ad-hoc sign the binary with the hypervisor entitlement, then exec it.
#
# Signing happens only when the binary is new to this script or changed since it last signed it
# (a stamp per binary path records the file's inode, size and mtime as signed), not on every run:
# `codesign --force` costs far more than the stat that tells it's already done, and cargo runs
# this for every test binary. Concurrent launches of one binary take a lock (mkdir is atomic) so
# one signs while the others wait, never exec a binary that's being rewritten, and then find the
# stamp fresh and skip signing.
set -e
bin="$1"
shift
here="$(cd "$(dirname "$0")" && pwd)"
ent="$here/hypervisor.entitlements"
stamps="${SIGN_RUN_STAMPS:-${TMPDIR:-/tmp}/sign-run-stamps-$(id -u)}"
mkdir -p "$stamps"
key="$(printf %s "$bin" | cksum | cut -d' ' -f1)"
stamp="$stamps/$key"
lock="$stamp.lock"

# What the binary looks like now; the stamp holds what it looked like when signed.
state() { stat -f '%i %z %Fm' "$bin" 2>/dev/null || echo missing; }
fresh() { [ "$(cat "$stamp" 2>/dev/null)" = "$(state)" ]; }

if ! fresh; then
  held=
  tries=0
  while :; do
    if mkdir "$lock" 2>/dev/null; then
      held=1
      trap 'rmdir "$lock" 2>/dev/null' EXIT INT TERM
      break
    fi
    fresh && break # another launch signed it while we waited
    tries=$((tries + 1))
    if [ "$tries" -gt 400 ]; then # ~20 s: a signer that died holding the lock
      rmdir "$lock" 2>/dev/null || true
      tries=0
    fi
    sleep 0.05
  done
  if [ -n "$held" ]; then
    status=0
    if ! fresh; then
      codesign --sign - --force --entitlements "$ent" "$bin" 2>/dev/null || status=$?
      if [ "$status" -eq 0 ]; then
        state >"$stamp.$$"
        mv -f "$stamp.$$" "$stamp"
      fi
    fi
    rmdir "$lock" 2>/dev/null || true
    trap - EXIT INT TERM
    [ "$status" -eq 0 ] || exit "$status"
  fi
fi
exec "$bin" "$@"
