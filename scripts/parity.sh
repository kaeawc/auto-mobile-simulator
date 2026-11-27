#!/usr/bin/env bash
# Compares simee against `xcrun simctl` on one booted device: identity, screenshot geometry and
# pixels, and app listing. Usage: scripts/parity.sh <udid> [simee binary]
set -euo pipefail
udid=${1:?usage: scripts/parity.sh <udid> [simee]}
sim=${2:-target/debug/simee}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok    $1"; else echo "FAIL  $1: simee=$2 simctl=$3"; fail=1; fi; }

ours=$("$sim" list --json | python3 -c "import json,sys;d=[x for x in json.load(sys.stdin) if x['udid']=='$udid'][0];print(d['name'],d['state'],d['runtime_id'],d['device_type_id'],sep='|')")
theirs=$(xcrun simctl list -j devices | python3 -c "
import json,sys
for rt,ds in json.load(sys.stdin)['devices'].items():
    for d in ds:
        if d['udid']=='$udid': print(d['name'],d['state'],rt,d['deviceTypeIdentifier'],sep='|')")
check identity "$ours" "$theirs"

"$sim" screenshot "$udid" "$tmp/ours.png"
# A device with two built-in screens (the iPhone Duo) needs one named: its main one is `primary`.
xcrun simctl io "$udid" screenshot --type=png "$tmp/theirs.png" >/dev/null 2>&1 ||
  xcrun simctl io "$udid" screenshot --type=png --display=primary "$tmp/theirs.png" >/dev/null 2>&1
size() { sips -g pixelWidth -g pixelHeight "$1" | awk '/pixel/{printf "%s ", $2}'; }
check "screenshot size" "$(size "$tmp/ours.png")" "$(size "$tmp/theirs.png")"
# Pixel difference: fraction of bytes that differ by more than 8 (the clock or a cursor may tick).
diff=$(python3 - "$tmp/ours.png" "$tmp/theirs.png" <<'PY'
import subprocess, sys
def raw(p):
    # The colour bytes only (BGR per pixel, no row padding), whatever the BMP's layout: simee
    # writes opaque screenshots as RGB PNGs (24 bpp BMP), simctl as RGBA (32 bpp).
    out = p + ".bmp"
    subprocess.run(["sips", "-s", "format", "bmp", p, "--out", out], capture_output=True, check=True)
    d = open(out, "rb").read()
    off, w, h = int.from_bytes(d[10:14], "little"), int.from_bytes(d[18:22], "little", signed=True), int.from_bytes(d[22:26], "little", signed=True)
    px = int.from_bytes(d[28:30], "little") // 8
    stride = (w * px + 3) // 4 * 4
    def bgr(row):
        if px == 3:
            return row
        out = bytearray(w * 3)
        out[0::3], out[1::3], out[2::3] = row[0::4], row[1::4], row[2::4]
        return bytes(out)
    return b"".join(bgr(d[off + r * stride: off + r * stride + w * px]) for r in range(abs(h)))
a, b = raw(sys.argv[1]), raw(sys.argv[2])
n = min(len(a), len(b))
print(f"{sum(1 for i in range(0, n, 97) if abs(a[i]-b[i]) > 8) / max(1, len(range(0, n, 97))):.3f}")
PY
)
if python3 -c "import sys; sys.exit(0 if float('$diff') < 0.02 else 1)"; then echo "ok    screenshot pixels (diff $diff)"; else echo "FAIL  screenshot pixels (diff $diff)"; fail=1; fi

ours_apps=$("$sim" apps "$udid" | sort | tr '\n' ' ')
theirs_apps=$(xcrun simctl listapps "$udid" 2>/dev/null | plutil -convert json -o - - | python3 -c "import json,sys;print(' '.join(sorted(json.load(sys.stdin))))")
check "installed apps" "$ours_apps" "$theirs_apps "
exit $fail
