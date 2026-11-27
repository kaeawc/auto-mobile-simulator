# Where software mode spends its time (4 vCPUs, no KVM)

Research for [#55](https://github.com/kaeawc/simee/issues/55): `emu -avd` on a Linux host with no
hardware virtualization and no GPU, profiled phase by phase so the next performance work goes to
the biggest cost. Status: in progress; sections are added as each step lands.

## 1. Environment

One host for every number below, 2026-11-27.

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, `/dev/kvm` absent, no GPU (Mesa llvmpipe/lavapipe only) |
| CPU | 4 vCPUs of an Intel Xeon at 2.2 GHz (`hypervisor`, `sse4_2`, `avx2`, `avx512f`; no `vmx`/`svm`) |
| Memory / disk | 7.8 GiB RAM, no swap; 9.8 GB root disk, 1.6 GB free after everything below |
| Toolchain | rustc 1.99.1 (rustup), release build of `emu` and `simee` from main at 5bcc3a8 |
| QEMU | 11.1.3 built from the release tarball (`--target-list=x86_64-softmmu --enable-slirp`, no docs, no GUI), `-j4` in 191 s, installed to a private prefix and passed as `EMU_QEMU`. Ubuntu 22.04's 6.2 is too old (docs/android.md) |
| Image | API 36 `default` x86_64, r2: `system.img` 1.9 GB, `vendor.img` 104 MB, kernel 20 MB (1.7 GB on disk) |
| Renderer | the SDK's Linux `libgfxstream_backend.so` 37.3.10 from `emu -provision-renderer` (255 MB on disk) |
| adb | platform-tools, 22 MB |
| AVD | an AVD named `prof` on that image: `hw.cpu.ncore=4`, `hw.ramSize=1536` (1.5 GB, to leave the 7.8 GiB host room for QEMU, emu, perf and the sampler), 720×1280 at 320 dpi, 2 GB data |
| Profiling | `perf record -a -g` (the 5.15 `perf` binary runs on this 6.1 kernel; `perf_event_paranoid` is -1) and a 2 s per-thread `/proc` sampler (`utime`+`stime` per QEMU/emu thread) |

Fetched, and only that: the system image zip, the renderer archive, platform-tools, the QEMU
source tarball (sha256 5a8289c7…84b22), the Rust toolchain. The documented route was used as
written, except that QEMU is built from source: no QEMU 11 package exists for Ubuntu 22.04.
