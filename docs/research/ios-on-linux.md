# iOS apps on Linux: open-source framework reimplementations

Can sim-rs run iOS apps on a Linux host, on top of a Rust reimplementation of the iOS frameworks?
This page surveys the open-source projects that have tried some part of this, says what each one
proves, and sketches what a Rust runtime for sim-rs would need, layer by layer.

- Scope: open-source projects and their published docs only. No Apple framework binaries were
  examined or decompiled. Apple's own *open-source* releases (objc4, dyld, XNU, CoreFoundation,
  libdispatch, Swift) count as open-source projects here.
- Date: November 2026. Project status is taken from READMEs, release pages and press; nothing was
  built or run for this survey. Claims that couldn't be checked against a primary source are marked.

## Summary

- **No open-source project runs modern iOS apps on Linux by reimplementing the frameworks.** The
  closest are touchHLE (Rust; real 32-bit iPhone OS 2–4 binaries, mostly games) and Darling
  (macOS, not iOS; CLI apps work, GUI is early after about 13 years). A research repo surveying the
  field in October 2026 reaches the same conclusion
  ([Legasitech/ios-emulator-linux](https://github.com/Legasitech/ios-emulator-linux)).
- **The only thing that runs modern iOS on Linux today is full-system emulation of Apple's own
  firmware**: [vphone-linux](https://github.com/YahiaAngelo/vphone-linux) boots iOS 26/27 in QEMU
  with a Rust paravirtual GPU and a Rust Metal→Vulkan translator. It's the opposite of a framework
  rewrite (it runs Apple's code, downloaded from Apple's IPSW servers), but two of its pieces are
  directly reusable.
- **touchHLE is the proof that the Rust approach works**: its own Objective-C runtime, dyld, libc,
  Foundation, UIKit, CoreGraphics, CoreAnimation, OpenGL ES and OpenAL are host Rust code; only the
  app binary runs on the emulated CPU. Its scope is the reason it's tractable: iOS 2–4, armv6/armv7
  only, 64-bit "never".
- **The right target for sim-rs is simulator builds, not App Store apps.** Our users are developers
  testing their own apps. A simulator build is unencrypted, built for the `IOSSIMULATOR` platform,
  and — on an aarch64 Linux host (or x86_64 with an x86_64 slice) — its machine code runs natively
  with no CPU emulation, the way Darling runs macOS binaries. That removes FairPlay, CPU emulation
  and the hardest legal questions in one step.
- **The cost is UIKit and SwiftUI.** Everything below them (loader, syscalls, libobjc, CF,
  libdispatch, Swift runtime, Foundation) has open-source prior art we can reuse or port. UIKit,
  QuartzCore's render model and SwiftUI have no complete open implementation in any language; that
  is where a Rust project would spend most of its effort.
- **"Omarchy" is not one of these projects.** [Omarchy](https://github.com/basecamp/omarchy) is
  DHH's opinionated Arch Linux + Hyprland distribution; it has nothing to do with Apple frameworks.
  (It shows up here only because vphone-linux tests its setup scripts on Arch/Omarchy.)

## What "an iOS app" means here

| Binary | Platform tag | Encrypted | CPU | Who has it | Fit for sim-rs on Linux |
| --- | --- | --- | --- | --- | --- |
| App Store / TestFlight app | `IOS` | Yes (FairPlay) | arm64 | End users | Poor: needs decryption, CPU emulation on x86 hosts, and every private behaviour of the device OS |
| Device build of your own app | `IOS` | No | arm64 | The developer | Possible, but platform-retag tricks are needed (below) |
| **Simulator build** | `IOSSIMULATOR` | No | arm64 or x86_64 | The developer (Xcode, xtool, CI) | **Best**: native execution on a matching host, same thing our macOS backend runs today |

Two facts make the simulator target attractive:

- A simulator build is a Darwin binary that links to frameworks by install name and symbol. It
  doesn't care who implements `UIView` as long as the symbols, the Objective-C/Swift ABI and the
  behaviour match. That's exactly the contract Darling fulfils for macOS binaries.
- Developers can produce simulator builds without a Mac: [xtool](https://github.com/xtool-org/xtool)
  builds SwiftPM packages into iOS apps on Linux and has recently added simulator support (per its
  author). It does this by extracting Xcode's SDK onto Linux, which raises the Xcode licence
  question covered under [Risks](#risks).

Device↔simulator retagging is a known technique, not a research problem: Project Zero showed how
iOS binaries run natively on macOS by patching the Mach-O platform
([Fuzzing iOS code on macOS at native speed](https://projectzero.google/2021/05/fuzzing-ios-code-on-macos-at-native.html)),
and [simforge](https://github.com/geekonion/simforge) rewrites the platform in decrypted arm64 app
binaries so they run in the Apple-silicon simulator (re-signing needed; extensions and sandbox
entitlements are known failure points). We don't need this, but it means a runtime that accepts
`IOSSIMULATOR` binaries can later accept developer device builds too.

## The projects

### Runs real iOS binaries on a reimplemented OS

| Project | Lang / licence | What it runs | Approach | Status (Nov 2026) | What it proves for us |
| --- | --- | --- | --- | --- | --- |
| [touchHLE](https://github.com/touchHLE/touchHLE) | Rust; MPL-2.0 source, GPL-3.0+ binaries | iPhone OS 2.x–iOS 4.0 apps, iPhone and iPad (v0.3.0, Oct 2026) | HLE: dynarmic runs the app's armv6/armv7 code; everything it calls is host Rust | Vast majority of apps still don't work; working ones are mostly games. Linux is build-from-source ("probably works") | A Rust ObjC runtime, dyld and framework set can run unmodified binaries |
| [Darling](https://github.com/darlinghq/darling) | C/ObjC/C++; GPL-3.0 (submodules vary) | macOS binaries | Mach-O loader (`mldr`) hands off to Apple's open-source dyld; `darlingserver` is a userspace kernel server for Mach IPC, POSIX and Darwin syscalls; frameworks reimplemented | CLI tools largely work; GUI "in active development"; initial Metal over Vulkan; ARM64 support and an AI-contribution policy reported in Nov 2026 (Phoronix; not verified at source). iOS listed only as a future aim | The kernel/loader half of the problem is solved and open; GUI frameworks are the long pole |
| [Cycada (Cider)](https://systems.cs.columbia.edu/projects/cycada) | Research (Columbia, ASPLOS 2014) | Unmodified iOS apps on Android | XNU compatibility in the Linux kernel ("duct tape"), Mach-O loading, diplomatic calls from iOS libraries into Android's GPU stack | Research prototype; slow; not maintained | Kernel-level XNU personality on Linux is feasible. It ran Apple's own iOS user-space libraries, so it is *not* a framework rewrite |
| [ipasim](https://github.com/ipasimulator/ipasim) | C++; thesis project | iOS apps on Windows | Emulates the app's ARM code; frameworks come from WinObjC compiled natively | Inactive | Mixed model (emulated app, native frameworks) works, but inherits WinObjC's gaps |
| HyperHLE / HyperHLE-Reborn | Rust; MPL-2.0 (per GameTechWiki) | touchHLE-era apps | touchHLE forks with game-specific compatibility work | Many unofficial forks, no canonical repo | Nothing new architecturally |
| [unidbg](https://github.com/zhkl0228/unidbg), [Chomper](https://github.com/sledgeh4w/chomper) | Java / Python | Single iOS functions and libraries | Unicorn CPU emulation, emulated dyld APIs, hooks | Reverse-engineering tools, iOS support "experimental" | Useful references for dyld API emulation; not an app runtime |

### Runs Apple's real iOS in a VM (for comparison)

| Project | Lang / licence | What it runs | Notes |
| --- | --- | --- | --- |
| [vphone-linux](https://github.com/YahiaAngelo/vphone-linux) | Glue MIT; QEMU fork GPL-3.0; GPU pieces GPL-2.0+/LGPL-3.0+ | iOS 26.6.2, 27.0.1, 27.1 (iPhone17,3, iPhone19,4) on Apple's `vresearch101` research platform | QEMU TCG (x86_64 hosts only; no arm64 KVM path yet), [reims-vgpu](https://github.com/steelbrain/reims-vgpu) (Rust paravirtual GPU), [metal2vulkan](https://github.com/steelbrain/metal2vulkan) (Rust, Metal AIR → Vulkan, beta). Boots to SpringBoard, built-in apps and Safari work; xtool-built IPAs install without signing; 50–70 fps scrolling, 4–20 s app launches. Needs IPSWs downloaded from Apple, ~16 GB RAM, 60–100 GB disk |
| [vphone-cli](https://github.com/Lakr233/vphone-cli) | — | The same virtual iPhone on Apple-silicon macOS via Virtualization.framework | The upstream that vphone-linux ports |
| [QEMU-t8030](https://github.com/TrungNguyen1909/qemu-t8030) / QEMUAppleSilicon | GPLv2 | iPhone 11 on iOS 14 betas | Research-grade; reached the lock screen (eShard) |
| [xnu-qemu-arm64](https://github.com/alephsecurity/xnu-qemu-arm64) | GPLv2 | iOS 12 kernel to a shell | Security research (2019) |

This family has perfect fidelity because it runs Apple's code, and that's also why it's outside
the brief: the frameworks aren't replaced, and the guest is Apple firmware under Apple's terms. It's
a useful yardstick (what "correct" looks like on Linux) and a source of two reusable Rust crates.

### Source-compatible reimplementations (recompile, don't run binaries)

These don't run existing binaries, but they are the largest bodies of open UIKit/SwiftUI/Cocoa
behaviour to learn from or port.

| Project | Lang / licence | Covers | Status |
| --- | --- | --- | --- |
| [WinObjC](https://github.com/microsoft/WinObjC) (Windows Bridge for iOS) | ObjC/C++; MIT | Foundation, UIKit, CoreGraphics, QuartzCore, CoreText, more — on Windows | Archived; criticised as incomplete, but the most complete open UIKit+CoreAnimation in ObjC |
| [GNUstep](https://www.gnustep.org/) (base, gui, [libobjc2](https://github.com/gnustep/libobjc2), Opal, [libs-uikit](https://github.com/gnustep/libs-uikit)) | ObjC; LGPL (libobjc2 MIT) | Foundation, AppKit, a new UIKit-style API on GNUstep | Active; libs-uikit is new and partly AI-written per its README |
| [Cocotron](https://cocotron.org/) | ObjC; MIT | Foundation (Linux/Windows), AppKit (Windows) | Dormant; Darling's AppKit is based on it |
| [Chameleon](https://github.com/BigZaphod/Chameleon) | ObjC; BSD | UIKit on top of AppKit + Core Animation | Dormant; depends on Apple's Core Animation |
| [UIKit-cross-platform](https://github.com/flowkey/UIKit-cross-platform) (flowkey) | Swift; MIT | UIKit subset on SDL_gpu, mainly Android | Last push July 2024; shipped in a production app |
| [Cacao](https://github.com/PureSwift/Cacao) (PureSwift) | Swift; MIT | UIKit-style API on Cairo + SDL2 (Linux) | Dormant |
| [OpenSwiftUI](https://github.com/OpenSwiftUIProject/OpenSwiftUI) + OpenGraph + OpenBox | Swift; MIT | SwiftUI, AttributeGraph, RenderBox | Active, WIP; core works on Apple platforms, not yet on Linux |
| [QuillUI](https://github.com/Lore-Hex/QuillUI) | Swift | SwiftUI, SwiftData, AppKit, UIKit source compatibility on Linux desktops (Qt) | New, active |
| [swift-cross-ui](https://github.com/stackotter/swift-cross-ui), Tokamak | Swift | SwiftUI-like APIs over GTK/Qt/web | Not ABI-compatible with SwiftUI |

### Building blocks

| Piece | Project | Lang / licence | Use for us |
| --- | --- | --- | --- |
| Objective-C runtime | Apple [objc4](https://github.com/apple-oss-distributions/objc4) | C++; APSL-2.0 | The exact Apple ABI (Darling builds it); APSL is OSI-approved but not GPL-compatible |
| | [libobjc2](https://github.com/gnustep/libobjc2) | C; MIT | GNUstep ABI, *not* Apple's Mach-O ABI; good reference, not a drop-in |
| | touchHLE `src/objc/` | Rust; MPL-2.0 | A Rust runtime with host-implemented classes (`objc_classes!`, `HostObject`); 32-bit, emulated-memory model |
| Loader | Apple [dyld](https://github.com/apple-oss-distributions/dyld) (APSL), Darling `mldr`, touchHLE `dyld.rs`, [maloader](https://github.com/shinh/maloader), [QBDL](https://github.com/quarkslab/QBDL), the `goblin` crate | various | Darling's split (tiny loader, then real dyld) is the proven path |
| Kernel personality | Darling `darlingserver`, Apple [XNU](https://github.com/apple-oss-distributions/xnu) (APSL) | C | Mach ports, BSD syscalls, `launchd`-style bootstrap; darlingserver is the reference |
| CoreFoundation | Apple CF (in [swift-corelibs-foundation](https://github.com/swiftlang/swift-corelibs-foundation)) | C; Apache-2.0 | Open, builds on Linux, CF types are toll-free-bridged to Foundation |
| Foundation | [swift-foundation](https://github.com/swiftlang/swift-foundation), swift-corelibs-foundation | Swift; Apache-2.0 | Swift Foundation API on Linux; *not* the ObjC `NSObject`-based classes apps link (Darling, GNUstep and Cocotron have those) |
| Dispatch | Apple [libdispatch](https://github.com/swiftlang/swift-corelibs-libdispatch) | C; Apache-2.0 | Builds on Linux today |
| Swift runtime | [swift](https://github.com/swiftlang/swift) stdlib/runtime | C++/Swift; Apache-2.0 | Needed for any Swift app; must be built for a Darwin-ABI target on Linux |
| Metal | [metal2vulkan](https://github.com/steelbrain/metal2vulkan) | Rust; GPL-2.0+ OR LGPL-3.0+ | Metal AIR → Vulkan pipelines plus a Metal API runtime; beta; drives iOS 27's own compositor in vphone-linux |
| | [Indium](https://github.com/darlinghq/indium) (Darling) | C++; 0BSD | Metal-like C++ API on Vulkan 1.3, Iridium (AIR → SPIR-V); internal library for an ABI-compatible Metal |
| GPU / display | [reims-vgpu](https://github.com/steelbrain/reims-vgpu) | Rust; GPL-2.0+ OR LGPL-3.0+ | Paravirtual GPU for a VM; reference for a host compositor |
| GL ES | ANGLE, Mesa | BSD / MIT | GLES for legacy apps; touchHLE translates GLES to host GL |
| CPU (only if needed) | dynarmic (touchHLE), Unicorn, FEX-Emu, box64 | various | Only for arm64 apps on x86_64 hosts; avoidable by targeting matching simulator slices |
| Signing | [rcodesign](https://github.com/indygreg/apple-platform-rs) | Rust; MPL-2.0 | Ad-hoc signing on Linux if we keep code-signature checks |

## What a Rust runtime for sim-rs would look like

The design that falls out of the prior art is Darling's process model with touchHLE's language and
attitude, aimed at simulator binaries:

```text
 sim-rs (sim-api) ─┬─ sim-ios-linux backend (new)
                   │     ├─ device "boot": a launchd-like root process + per-app processes
                   │     ├─ display: render server → <device>/display.sock (sim_core::stream)
                   │     └─ input: sim-core gestures/HID → event queue → UIApplication
                   └─ app process (native arm64/x86_64 Mach-O, IOSSIMULATOR)
                         ├─ loader (Rust): map Mach-O, bind to replacement dylibs by install name
                         ├─ Darwin personality (Rust): syscalls, Mach ports, TLS, signals, commpage
                         ├─ libobjc (Apple ABI) + Swift runtime + CF + libdispatch (ported C/C++)
                         └─ frameworks (Rust, exported with Apple symbol names and ABI):
                              Foundation (ObjC classes), UIKit, QuartzCore, CoreGraphics,
                              CoreText, ImageIO, Metal (metal2vulkan/Indium), SwiftUI, ...
```

| Layer | Hardest part | Prior art | Rust plan |
| --- | --- | --- | --- |
| Mach-O loading, binding, chained fixups, TLVs | Modern dyld features (chained fixups, shared-cache assumptions, `__DATA_CONST`) | dyld (APSL), touchHLE `dyld.rs`, `goblin` | Pure-logic parsing and fixup planning in a `sim-core`-style crate with unit tests; mapping in a thin FFI crate |
| Darwin syscalls on Linux | `mach_msg`, ports and rights, `bsdthread_*`, `kevent`, `ulock`, commpage | darlingserver | Start in-process (no server): a syscall shim library that the replacement libSystem calls into; a server only when cross-process Mach IPC is needed (launchd, XPC) |
| libSystem | Hundreds of libc/pthread/malloc symbols with Darwin semantics and errno values | Darling's libSystem (Apple OSS) | Mostly forwarders to Linux libc with ABI/struct translation (`stat`, `dirent`, errno) |
| Objective-C runtime | Apple ABI: `objc_msgSend` fast path, non-fragile ivars, tagged pointers, category loading from Mach-O sections, ARC entry points, `@autoreleasepool` | objc4 (APSL), touchHLE (Rust, 32-bit) | Either build objc4 as-is (fastest route, APSL) or write a Rust runtime with an assembly `objc_msgSend`; framework classes defined in Rust and registered at load |
| Swift | Runtime metadata, Darwin-specific ObjC interop in the stdlib | swift (Apache-2.0) | Build Apple's Swift runtime for the target ABI; don't reimplement |
| CoreFoundation, Dispatch | Bridging CF↔NS objects, run loops tied to Mach ports | CF and libdispatch (Apache-2.0) | Port; replace Mach-port run-loop sources with our shim |
| Foundation (ObjC) | Hundreds of classes apps touch on launch (bundles, user defaults, URL loading, notifications, KVO) | Darling, GNUstep base, Cocotron, WinObjC; swift-foundation for the Swift side | Implement in Rust incrementally, driven by real apps' imports |
| UIKit | Layout, responder chain, gestures, scroll physics, text input, view controllers, scenes, accessibility — behaviour, not just API | WinObjC, flowkey, Chameleon, touchHLE (old iOS) | The bulk of the project. Build against tests that compare with our macOS backend (same app, same screenshots) |
| QuartzCore / render server | Layer tree, implicit animations, commit/transaction model, compositing off the main thread | WinObjC, reims-vgpu as a compositor reference | A Rust render server (wgpu/Vulkan) that produces frames for `sim_core::stream`, so the desktop app and AutoMobile work unchanged |
| CoreGraphics / CoreText / ImageIO | Pixel-exact 2D rendering and text shaping | Opal (Cairo), WinObjC, touchHLE | tiny-skia/vello for paths, swash/rustybuzz for text, `image`/`png` for ImageIO |
| Metal | Shader translation from AIR, argument buffers | metal2vulkan, Indium | Reuse; licence permits (LGPL-3.0 option, 0BSD) |
| SwiftUI | AttributeGraph semantics, layout engine, ABI-compatible `View` protocol witnesses | OpenSwiftUI, OpenGraph, OpenBox | Track/contribute to OpenSwiftUI rather than start over |
| Accessibility tree | AutoMobile reads the view hierarchy | none | Free win: our own UIKit can expose the hierarchy directly, better than the macOS backend can |

### A plausible order of work

1. **Hello, Darwin.** A C command-line `IOSSIMULATOR` arm64 binary (built with xtool's SDK)
   runs on aarch64 Linux: loader, fixups, libSystem forwarders, `printf`, `exit`.
2. **Objective-C and CF.** An ObjC CLI that uses `NSString`, `NSArray`, `NSRunLoop`,
   `dispatch_async`: libobjc, CF, libdispatch, a first slice of Foundation.
3. **Swift.** A Swift CLI with `print` and `Foundation.Date`: the Swift runtime and overlays.
4. **One window.** `UIApplicationMain`, a `UIWindow`, a `UIView` with a background colour and a
   `UILabel`, composited by a Rust render server into `display.sock`; taps from sim-core gestures.
   At this point the existing desktop window and CLI screenshots work against a Linux device.
5. **Real apps.** Pick two or three representative open-source iOS apps and implement what they
   import, with screenshot diffs against the macOS backend running the same simulator build.
6. **SwiftUI and Metal.** OpenSwiftUI on our UIKit/QuartzCore; metal2vulkan for Metal apps and
   for any SwiftUI rendering that goes through Metal.

Steps 1–3 are well-trodden (Darling has done all of them for macOS). Step 4 onward is where no
open project has gone for modern iOS.

## Risks

- **Scope.** Darling, with a decade and more of work and Apple's open-source low layers, still has
  "very limited" GUI support for macOS. touchHLE succeeds by limiting itself to 2008–2010 APIs. A
  modern UIKit/SwiftUI app touches far more surface. Expect a long tail where each new app needs new
  behaviour, not just new symbols.
- **Behavioural fidelity.** Matching the stock tools "exactly", as the rest of sim-rs aims to, is
  much harder here than on Android (where emu-rs boots the real system image). Layout, text
  metrics, animation timing and scroll physics will visibly differ from Apple's until measured and
  tuned against the macOS backend.
- **SDK and licences.** Building simulator apps needs Apple's SDK; Xcode's licence ties its use to
  Apple-branded hardware, which xtool-on-Linux sidesteps rather than resolves. Running a binary the
  developer built is a cleaner position than distributing anything from the SDK. Our runtime must
  not ship Apple headers, `.tbd` files or binaries; symbol and class names are exported because apps
  reference them (the same position Darling, WinObjC and GNUstep take). This is not legal advice.
- **Licence mixing.** sim-rs is MIT. touchHLE is MPL-2.0 (file-level copyleft: reusable with care),
  Darling is GPL-3.0 (reference only unless we change licence), objc4/dyld/XNU are APSL-2.0,
  Swift/CF/libdispatch are Apache-2.0, metal2vulkan and reims-vgpu are GPL-2.0+ OR LGPL-3.0+ (the
  LGPL option allows linking from MIT code), Indium is 0BSD.
- **App Store apps stay out of reach.** FairPlay-encrypted binaries need a decryption step we
  shouldn't build. Keep the product promise to "apps you build".

## Recommendation

1. Treat a Linux iOS backend as a separate, long-horizon track, not a port of `sim-coresim`. The
   macOS backend stays the reference implementation, and the Linux backend is measured against it.
2. Target `IOSSIMULATOR` arm64 builds on aarch64 Linux first (native speed, no CPU emulator), with
   x86_64 simulator slices on x86_64 hosts as a second configuration.
3. Reuse, don't rewrite, the layers Apple open-sourced (CF, libdispatch, Swift runtime, and objc4
   if APSL is acceptable). Spend the Rust effort where nothing exists: the Darwin personality,
   Foundation's ObjC classes, UIKit, QuartzCore and the render server.
4. Borrow concrete designs: Darling's loader/dyld split and darlingserver for the kernel layer;
   touchHLE's host-implemented ObjC classes in Rust; metal2vulkan or Indium for Metal; OpenSwiftUI
   for SwiftUI.
5. Prove the shape with step 1–4 above before committing: a single UIKit window streamed into the
   existing desktop app on Linux would show whether the architecture holds.

## Sources

- touchHLE: [repository](https://github.com/touchHLE/touchHLE), [releases](https://github.com/touchHLE/touchHLE/releases), [Wikipedia](https://en.wikipedia.org/wiki/TouchHLE)
- Darling: [repository](https://github.com/darlinghq/darling), [docs: loader](https://docs.darlinghq.org/internals/basics/loader.html), [Q2 2023 progress report (darlingserver)](https://blog.darlinghq.org/2023/08/21/progress-report-q2-2023), [Phoronix, Nov 2026](https://phoronix.com/news/Darling-Using-AI-macOS-Linux), [Indium](https://github.com/darlinghq/indium)
- vphone-linux: [repository](https://github.com/YahiaAngelo/vphone-linux), [v0.5](https://github.com/YahiaAngelo/vphone-linux/releases/tag/v0.5), [reims-vgpu](https://github.com/steelbrain/reims-vgpu), [metal2vulkan](https://github.com/steelbrain/metal2vulkan), [InfoQ on vphone-cli and iOS 27 virtualization](https://www.infoq.com/news/2026/09/ios-27-virtualization/)
- Full-system emulation: [qemu-t8030](https://github.com/TrungNguyen1909/qemu-t8030), [eShard: emulating iOS 14 with QEMU](https://eshard.com/blog/emulating-ios-14-with-qemu), [xnu-qemu-arm64](https://alephsecurity.com/2019/06/17/xnu-qemu-arm64-1/)
- Cider/Cycada: [project](https://systems.cs.columbia.edu/projects/cycada), [ASPLOS 2014 paper](https://www.cs.columbia.edu/~alduaij/pubs/cider-asplos2014.pdf)
- [ipasim](https://github.com/ipasimulator/ipasim), [WinObjC announcement](https://blogs.windows.com/windowsdeveloper/2015/08/06/windows-bridge-for-ios-lets-open-this-up/)
- [unidbg](https://github.com/zhkl0228/unidbg), [maloader](https://github.com/shinh/maloader), [QBDL](https://github.com/quarkslab/QBDL)
- [Project Zero: iOS apps on macOS](https://projectzero.google/2021/05/fuzzing-ios-code-on-macos-at-native.html), [simforge](https://github.com/geekonion/simforge)
- [xtool](https://github.com/xtool-org/xtool), [Swift Forums announcement](https://forums.swift.org/t/xtool-cross-platform-xcode-replacement-build-ios-apps-on-linux-and-more/79803)
- UI reimplementations: [OpenSwiftUI](https://github.com/OpenSwiftUIProject/OpenSwiftUI), [QuillUI](https://github.com/Lore-Hex/QuillUI), [UIKit-cross-platform](https://github.com/flowkey/UIKit-cross-platform), [GNUstep libs-uikit](https://github.com/gnustep/libs-uikit), [Cocotron](https://cocotron.org/), [swift-cross-ui](https://github.com/stackotter/swift-cross-ui)
- Apple open source on Linux: [swift-foundation](https://github.com/swiftlang/swift-foundation), [swift-corelibs-foundation](https://github.com/swiftlang/swift-corelibs-foundation), [Swift core libraries](https://swift.org/documentation/core-libraries/)
- Rust/ObjC: [objc2 / objc-sys](https://docs.rs/objc-sys/)
- Field survey: [Legasitech/ios-emulator-linux](https://github.com/Legasitech/ios-emulator-linux), [Emulation General Wiki: iOS emulators](https://emulation.gametechwiki.com/index.php/iOS_emulators)
- Omarchy: [basecamp/omarchy](https://github.com/basecamp/omarchy)
