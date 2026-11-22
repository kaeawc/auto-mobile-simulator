#!/bin/sh
# Builds an SDK overlay whose `emulator/emulator` is emu-rs, so tools that locate the emulator
# through ANDROID_HOME (auto-mobile, Gradle managed devices, scripts) launch emu-rs unchanged.
#
#   scripts/sdk-shim.sh [shim-dir]          # default: ~/.emu-rs/sdk
#   ANDROID_HOME=~/.emu-rs/sdk ANDROID_SDK_ROOT=~/.emu-rs/sdk <tool>
#
# Everything else (platform-tools, system-images, build-tools, ...) links to the real SDK.
set -eu

repo="$(cd "$(dirname "$0")/.." && pwd)"
real="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Library/Android/sdk}}"
shim="${1:-$HOME/.emu-rs/sdk}"

[ -d "$real" ] || { echo "real SDK not found at $real" >&2; exit 1; }
case "$shim" in "$real"*) echo "shim dir must not be inside the real SDK" >&2; exit 1 ;; esac

cargo build --release -p emu --manifest-path "$repo/Cargo.toml"
mkdir -p "$shim/emulator"
for entry in "$real"/*; do
    name="$(basename "$entry")"
    [ "$name" = emulator ] && continue
    ln -sfn "$entry" "$shim/$name"
done
# Keep the stock emulator's helper tools (e2fsck, resize2fs, ...) for tools that look for them;
# emu-rs itself formats userdata natively and needs none of them.
[ -d "$real/emulator/bin64" ] && ln -sfn "$real/emulator/bin64" "$shim/emulator/bin64"
# Host rendering: the gfxstream renderer and its Vulkan loader/ICDs (MoltenVK, lavapipe).
[ -d "$real/emulator/lib64" ] && ln -sfn "$real/emulator/lib64" "$shim/emulator/lib64"
cp "$repo/target/release/emu" "$shim/emulator/emulator"
codesign --sign - --force --entitlements "$repo/scripts/hypervisor.entitlements" "$shim/emulator/emulator"
printf 'Pkg.Path=emulator\nPkg.Desc=emu-rs\nPkg.Revision=0.1.0\n' > "$shim/emulator/source.properties"
echo "emu-rs SDK overlay ready: export ANDROID_HOME=$shim ANDROID_SDK_ROOT=$shim"
