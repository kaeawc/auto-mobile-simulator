#!/usr/bin/env bash
# The KVM backend's boot smoke test: emu-vmm boots an arm64 Linux kernel on the stock board under
# KVM, and the guest powers itself off (PSCI SYSTEM_OFF) once its init has run on every vCPU.
#
#   scripts/kvm-smoke.sh            # on any x86_64 or arm64 Linux host, no KVM needed: KVM runs
#                                   # inside an emulated arm64 machine with EL2 (QEMU TCG,
#                                   # virt,virtualization=on,gic-version=3). Slow, correct.
#   scripts/kvm-smoke.sh --native   # on an arm64 Linux host with /dev/kvm: boots directly.
#
# Needs: cargo, curl, dpkg-deb, python3; for the emulated host also qemu-system-aarch64 and, on an
# x86_64 host, an aarch64 cross linker (gcc-aarch64-linux-gnu). The guest kernel and busybox are
# Ubuntu 24.04's arm64 packages, pinned by SHA-256 and cached in $SMOKE_DIR.
#
# Measures nothing worth publishing when emulated: the nested machine runs ~50x slower than
# hardware. It proves the backend boots (vGICv3, PSCI CPU_ON of the secondaries, the arch timer,
# MMIO, ioeventfd notifies, system events), not how fast.
set -euo pipefail
cd "$(dirname "$0")/.."

native=0
[[ ${1:-} == --native ]] && native=1
cpus=${SMOKE_CPUS:-2}
dir=${SMOKE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/simee/kvm-smoke}
timeout=${SMOKE_TIMEOUT:-1200}
mkdir -p "$dir"

ports=http://ports.ubuntu.com/ubuntu-ports/pool/main
kernel_deb=linux-image-unsigned-6.8.0-31-generic_6.8.0-31.31_arm64.deb
kernel_sha=370c09d374978f7bd6b157adc8dfae9b4498d9a4ae24aa8068000f0c61a0273e
busybox_deb=busybox-static_1.36.1-6ubuntu3_arm64.deb
busybox_sha=1a8c03948f99dfbb90efd93d9214418b416ac0737ed4c90938ebbcd645f10682

fetch() { # url file sha256
  local f=$dir/$2
  if [[ ! -f $f ]] || ! echo "$3  $f" | sha256sum -c --quiet - 2>/dev/null; then
    curl -sSfL --retry 3 -o "$f.part" "$1"
    echo "$3  $f.part" | sha256sum -c --quiet - || { echo "kvm-smoke: $2: checksum mismatch" >&2; exit 1; }
    mv "$f.part" "$f"
  fi
}
fetch "$ports/l/linux/$kernel_deb" "$kernel_deb" "$kernel_sha"
fetch "$ports/b/busybox/$busybox_deb" "$busybox_deb" "$busybox_sha"
rm -rf "$dir/kernel" "$dir/busybox"
dpkg-deb -x "$dir/$kernel_deb" "$dir/kernel"
dpkg-deb -x "$dir/$busybox_deb" "$dir/busybox"
image=$(ls "$dir"/kernel/boot/vmlinuz-*)

# emu-vmm's boot_kernel example, static so it runs in a bare initramfs.
target=aarch64-unknown-linux-gnu
if [[ $(uname -m) != aarch64 ]]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}
fi
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" \
  cargo build --release --locked --target $target -p emu-vmm --example boot_kernel
boot_kernel=${CARGO_TARGET_DIR:-target}/$target/release/examples/boot_kernel

# newc initramfs archives: (path in archive, host file or "" for a directory, mode).
mkcpio() { # out, then triples on stdin
  python3 -I -c "$(cat <<'PY'
import os, sys
out = open(sys.argv[1], "wb")
def entry(name, data, mode):
    hdr = "070701" + "".join("%08x" % v for v in (0, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name) + 1, 0))
    out.write(hdr.encode() + name.encode() + b"\0")
    out.write(b"\0" * ((4 - (110 + len(name) + 1) % 4) % 4))
    out.write(data)
    out.write(b"\0" * ((4 - len(data) % 4) % 4))
for line in sys.stdin.read().splitlines():
    name, src, mode = line.split("\t")
    if src:
        entry(name, open(src, "rb").read(), int(mode, 8))
    else:
        entry(name, b"", 0o040755)
entry("TRAILER!!!", b"", 0)
PY
)" "$1"
}

bb=$dir/busybox/usr/bin/busybox
[[ -x $bb ]] || bb=$dir/busybox/bin/busybox
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The guest under test: reports each online CPU, then powers off.
cat > "$work/guest-init" <<'EOF'
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t devtmpfs dev /dev
# 2000 lines through virtio-console's transmit queue: each batch is a QueueNotify, which only
# completes (and frees the queue for the next) if the notify reaches the device.
i=0; while [ $i -lt 2000 ]; do echo "line $i" > /dev/hvc0 || break; i=$((i + 1)); done
echo "kvm-smoke guest: wrote $i lines to virtio-console"
echo "kvm-smoke guest: up on $(/bin/busybox grep -c ^processor /proc/cpuinfo) cpus, $(/bin/busybox cut -d' ' -f1 /proc/uptime) s"
/bin/busybox poweroff -f
EOF
printf '%s\t%s\t%s\n' bin "" 0 proc "" 0 dev "" 0 bin/busybox "$bb" 100755 init "$work/guest-init" 100755 | mkcpio "$work/guest.cpio"

log=$dir/last.log
guest_ok() { grep -q "kvm-smoke guest: wrote 2000 lines" "$log" && grep -q "kvm-smoke guest: up on $cpus cpus" "$log"; }
if [[ $native == 1 ]]; then
  timeout "$timeout" "$boot_kernel" "$image" "$work/guest.cpio" "$cpus" 2>&1 | tee "$log"
  [[ ${PIPESTATUS[0]} == 0 ]] && guest_ok
  exit
fi

# The emulated KVM host: boots, runs boot_kernel on /dev/kvm, prints its verdict, powers off.
cat > "$work/host-init" <<EOF
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sys /sys
/bin/busybox mount -t devtmpfs dev /dev
/bin/busybox ls -l /dev/kvm || echo "kvm-smoke host: no /dev/kvm"
if /boot_kernel /Image /guest.cpio $cpus; then echo "kvm-smoke: PASS"; else echo "kvm-smoke: FAIL (\$?)"; fi
/bin/busybox poweroff -f
EOF
printf '%s\t%s\t%s\n' bin "" 0 proc "" 0 sys "" 0 dev "" 0 bin/busybox "$bb" 100755 init "$work/host-init" 100755 \
  boot_kernel "$boot_kernel" 100755 Image "$image" 100644 guest.cpio "$work/guest.cpio" 100644 | mkcpio "$work/host.cpio"

timeout "$timeout" qemu-system-aarch64 -M virt,virtualization=on,gic-version=3 -cpu max -smp $((cpus + 1)) -m 2048 \
  -accel tcg,thread=multi -nographic -nic none -no-reboot -kernel "$image" -initrd "$work/host.cpio" \
  -append "console=ttyAMA0 rdinit=/init panic=-1" | tee "$log"
guest_ok && grep -q "kvm-smoke: PASS" "$log"
