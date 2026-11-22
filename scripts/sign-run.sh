#!/bin/sh
# cargo runner: ad-hoc sign the binary with the hypervisor entitlement, then exec it.
set -e
bin="$1"
shift
ent="$(cd "$(dirname "$0")" && pwd)/hypervisor.entitlements"
codesign --sign - --force --entitlements "$ent" "$bin" 2>/dev/null
exec "$bin" "$@"
