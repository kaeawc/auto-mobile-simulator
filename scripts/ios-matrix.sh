#!/usr/bin/env bash
# Runs every iOS feature simee has against one booted iPhone or iPad and prints a line per
# feature: pass or fail with the error. docs/ios-versions.md is this, once per runtime.
#
# Usage: scripts/ios-matrix.sh <udid> [simee binary]
# Builds a one-screen test app (Xcode's swiftc) to install, and serves the API on a private
# socket for the methods the CLI has no subcommand for. Run it a minute after boot: right after,
# `push` can fail and the screenshot can still be animating.
set -uo pipefail
udid=${1:?usage: scripts/ios-matrix.sh <udid> [simee]}
sim=${2:-target/debug/simee}
tmp=$(mktemp -d)
sock="$tmp/s.sock"
server=
cleanup() {
  [ -n "$server" ] && kill "$server" 2>/dev/null
  rm -rf "$tmp"
}
trap cleanup EXIT

app="$tmp/Tiny.app"
mkdir -p "$app"
cat >"$tmp/main.swift" <<'EOF'
import UIKit
class D: UIResponder, UIApplicationDelegate {
  var window: UIWindow?
  func application(_ a: UIApplication, didFinishLaunchingWithOptions o: [UIApplication.LaunchOptionsKey: Any]?) -> Bool {
    window = UIWindow(frame: UIScreen.main.bounds)
    let vc = UIViewController(); vc.view.backgroundColor = .systemTeal
    window?.rootViewController = vc; window?.makeKeyAndVisible(); return true
  }
}
UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil, NSStringFromClass(D.self))
EOF
xcrun -sdk iphonesimulator swiftc -target arm64-apple-ios17.0-simulator "$tmp/main.swift" -o "$app/Tiny" || exit 1
plutil -create xml1 "$app/Info.plist"
for kv in CFBundleIdentifier=dev.simee.tiny CFBundleExecutable=Tiny CFBundleName=Tiny CFBundlePackageType=APPL \
  CFBundleShortVersionString=1.0 CFBundleVersion=1 MinimumOSVersion=17.0; do
  plutil -insert "${kv%%=*}" -string "${kv#*=}" "$app/Info.plist"
done
plutil -insert UIDeviceFamily -json '[1,2]' "$app/Info.plist"
plutil -insert CFBundleSupportedPlatforms -json '["iPhoneSimulator"]' "$app/Info.plist"
plutil -insert UILaunchScreen -json '{}' "$app/Info.plist"
codesign -s - -f "$app" 2>/dev/null

"$sim" serve --socket "$sock" >"$tmp/serve.log" 2>&1 &
server=$!
for _ in $(seq 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done

fails=0
line() { printf '%-20s %s\n' "$1" "$2"; }
run() { # name, then a simee command line
  local name=$1 out
  shift
  if out=$("$sim" --in-process "$@" 2>&1); then
    line "$name" pass
  else
    line "$name" "fail: $(echo "$out" | head -1)"
    fails=$((fails + 1))
  fi
}
call() { # name method params
  local out
  if out=$("$sim" call --socket "$sock" "$2" "$3" 2>&1); then
    line "$1" pass
  else
    line "$1" "fail: $(echo "$out" | head -1)"
    fails=$((fails + 1))
  fi
  LAST=$out
}
d="\"device\":\"$udid\""

if out=$("$(dirname "$0")/parity.sh" "$udid" "$sim" 2>&1); then
  line parity pass
else
  line parity "fail: $(echo "$out" | grep FAIL | head -1)"
  fails=$((fails + 1))
fi
run screenshot screenshot "$udid" "$tmp/shot.png"
run tap tap "$udid" 100 100
run swipe swipe "$udid" 600 1800 600 1200 --duration 200
run button button "$udid" home
run key key "$udid" 40
run open-url open-url "$udid" https://example.com
run type type "$udid" "hello simee"
run appearance appearance "$udid" dark
run appearance appearance "$udid" light
run location location "$udid" 37.33 -122.03
run memory-warning memory-warning "$udid"
run install install "$udid" "$app"
run launch launch "$udid" dev.simee.tiny
run terminate terminate "$udid" dev.simee.tiny
run privacy privacy "$udid" grant photos dev.simee.tiny
run privacy privacy "$udid" revoke photos dev.simee.tiny
run uninstall uninstall "$udid" dev.simee.tiny
run rotate rotate "$udid" --orientation landscape-left
run rotate rotate "$udid" --orientation portrait
run exec exec "$udid" defaults read -g AppleLocale
call status-bar status_bar "{$d,\"time\":\"9:41\"}"
call status-bar clear_status_bar "{$d}"
call content-size content_size "{$d,\"size\":\"large\"}"
call biometric biometric "{$d,\"action\":\"enroll\"}"
call push push "{$d,\"bundle_id\":\"com.apple.Preferences\",\"payload\":{\"aps\":{\"alert\":\"hi\"}}}"
call keychain reset_keychain "{$d}"
call processes processes "{$d}"
call logs logs "{$d,\"last\":\"5s\"}"
call connections connections "{$d}"
call containers containers "{$d}"
call net-proxy network_shape "{$d,\"preset\":\"lte\"}"
call net-inject network_shape "{$d,\"preset\":\"3g\",\"inject\":true}"
call net-clear network_clear "{$d}"
call audio-state audio_state "{$d}"
call audio-volume audio_volume "{$d,\"level\":100}"
call audio-capture audio_capture_start "{$d}"
cap=$(echo "$LAST" | python3 -c "import json,sys;print(json.load(sys.stdin).get('capture_id',''))" 2>/dev/null)
[ -n "$cap" ] && call audio-capture audio_capture_stop "{\"capture_id\":\"$cap\"}"
call audio-mute audio_mute "{$d,\"on\":true}"
call audio-mute audio_mute "{$d,\"on\":false}"
call carplay carplay "{$d,\"on\":true}"
call carplay carplay "{$d,\"on\":false}"
echo "$fails failed"
[ "$fails" = 0 ]
