#!/usr/bin/env bash
# Builds simee's iOS camera shim and its fixture app for the iOS simulator (arm64), with the
# Xcode toolchain only (xcrun clang; nothing is downloaded), ad-hoc signed:
#
#   shims/ios-camera/build.sh [OUT_DIR]     # default: target/ios-camera
#
#   OUT_DIR/libsimee-camera.dylib        the library to insert (DYLD_INSERT_LIBRARIES); links only
#                                        libSystem and loads the hooks into user apps
#   OUT_DIR/libsimee-camera-hooks.dylib  the AVFoundation hooks (must stay beside the first)
#   OUT_DIR/CameraFixture.app            the fixture app (bundle id dev.simee.camerafixture)
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
out=${1:-$here/../../target/ios-camera}
mkdir -p -- "$out/CameraFixture.app"
out=$(cd -- "$out" && pwd)
target=arm64-apple-ios17.0-simulator
cc=(xcrun --sdk iphonesimulator clang -target "$target" -O2 -Wall -Werror)

"${cc[@]}" -dynamiclib -install_name @rpath/libsimee-camera.dylib \
  "$here/loader.c" -o "$out/libsimee-camera.dylib"
"${cc[@]}" -dynamiclib -fobjc-arc -install_name @rpath/libsimee-camera-hooks.dylib \
  -framework Foundation -framework UIKit -framework AVFoundation -framework CoreImage \
  -framework CoreMedia -framework CoreVideo -framework CoreGraphics -framework QuartzCore \
  "$here/hooks.m" -o "$out/libsimee-camera-hooks.dylib"
"${cc[@]}" -fobjc-arc -framework Foundation -framework UIKit -framework AVFoundation \
  -framework CoreMedia -framework CoreVideo -framework QuartzCore \
  "$here/fixture/main.m" -o "$out/CameraFixture.app/CameraFixture"
cp -- "$here/fixture/Info.plist" "$out/CameraFixture.app/Info.plist"

codesign -f -s - "$out/libsimee-camera.dylib" "$out/libsimee-camera-hooks.dylib" 2>/dev/null
codesign -f -s - "$out/CameraFixture.app" 2>/dev/null
echo "$out"
