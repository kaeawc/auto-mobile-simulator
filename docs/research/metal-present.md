# Metal present path: the kill gate

Can emu hand a posted frame to simee as a Metal or IOSurface object instead of reading it back
to the CPU? And does that save enough CPU per frame to be worth building? Issue #356. The concept
review chose option (a), a present path outside gfxstream, and asked for this spike before any of
it is wired in. The spike uses no guest and no VM. Its kill criteria:

- no usable handle; or
- the path saves under 1 ms per frame on a phone **and** under 10% of a core while streaming a
  tablet.

**Verdict: kill.** The handle is usable on both Vulkan drivers, the bytes come out exact, and a
GPU copy into an IOSurface works. But the CPU saved is 0.39 ms per frame on a phone (1080×2400)
and 0.60 ms per frame on a tablet (2560×1600), or 3.6% of a core at 60 fps. Both are well under
the bars.

## Method

`crates/emu-gfxstream/examples/metal_handle.rs` initializes the renderer in-process the way
`emu` does for an API 36 guest: Vulkan only, no GL, Vulkan composition, emu's features. It runs
on the driver named on its command line. It then creates color buffers of 720×1280, 1080×2400 and
2560×1600 through `stream_renderer_resource_create`, and uploads a known pattern into each with
`stream_renderer_transfer_write_iov`. For each buffer it:

1. **Reads the buffer back** with the renderer's private direct Vulkan readback,
   `VkEmulation::readColorBufferToBytes`, which is emu's frame readback (`VkRead` in
   `crates/emu-gfxstream/src/lib.rs`). It checks the bytes against the upload.
2. **Resolves and calls `VkEmulation::getColorBufferMetalMemoryHandle(unsigned)`**, also private,
   and inspects what it returns with the Objective-C runtime: the class name,
   `conformsToProtocol:` for `MTLResource`, `MTLHeap`, `MTLTexture`, `MTLBuffer` and
   `MTLAllocation`, the heap's or texture's own properties, and `-iosurface` for a texture.
3. **Aliases the color buffer's image**, because the handle is a placement heap.
   `-[MTLHeap newTextureWithDescriptor:offset:0]` with the image's description (RGBA8Unorm, its
   size, the heap's storage, cache and hazard modes) gives a texture over the same memory. Several
   `MTLTextureUsage` sets were tried, since usage changes the layout Metal picks.
4. **Copies the alias into an IOSurface-backed `MTLTexture`** on its own `MTLCommandQueue`, two
   ways:
   - a blit (`copyFromTexture:toTexture:`) into an RGBA surface;
   - a one-line compute kernel into a BGRA surface. The encoder's pixel buffers are BGRA because
     CoreVideo makes no IOSurface-backed RGBA ones (`sim_coresim::videotoolbox::PixelFormat`).
     A blit can't swap channels; a kernel writing a BGRA texture does it for free.

   It compares each surface's bytes with the readback's.
5. **Repeats the comparison with a screen-like picture** of flat 64-pixel blocks and gradients,
   which Apple GPUs would compress losslessly where they can't compress noise. All the timings
   then use that picture.
6. **Times 200 runs of each** (after one warm-up comparison) and takes the median:
   - the readback;
   - the blit and the kernel, each committed and waited for;
   - the encoder's RGBA-to-BGRA copy that a read-back frame needs today
     (`sim_core::pixels::swap_red_blue_into`, what `PixelBufferPool::fill` runs for RGBA).

   Each run records wall time, this thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`) and the GPU's
   time (`GPUEndTime - GPUStartTime`). The process's CPU for each loop (`getrusage`, every
   thread, the driver's included) is divided by 200.

The private symbols are found the way the library finds `VkRead`, with the minimal lookup
duplicated in the example because the library's `macho` module is private:

- the backend file's Mach-O symbol table gives the unslid addresses;
- they are used only when the file's `LC_UUID` is the loaded image's;
- the slide is the one two exported symbols (`FrameBuffer::getFB`, `hasEmulationVk`) agree on;
- the live `VkEmulation` comes from the two loads that `FrameBuffer::hasEmulationVk()` makes,
  used only when its code is exactly that accessor.

The disassembly of `getColorBufferMetalMemoryHandle` shows an instance method: it locks the
emulation's mutex, finds the color buffer in its map, and returns one pointer field (null if it
isn't found). That field is filled by `VkEmulation::getMtlResourceFromVkDeviceMemory`, which calls
`vkGetMemoryMetalHandleEXT` with `VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT` (0x40000). So
the handle is the color buffer's `VkDeviceMemory` as a heap, and it is only there when gfxstream
picks the `Metal` external-memory mode. It logged "Using Vulkan externalMemoryMode: Metal for
VkEmulation" on both drivers.

Run it like this (`gles_angle` on the fallback path: the example has no rpath to it, see
"Side findings"):

```text
cargo build --release -p simee-emu-gfxstream --example metal_handle
L=~/Library/Android/sdk/simee/renderer/current/lib64
DYLD_FALLBACK_LIBRARY_PATH=$L/gles_angle:/usr/lib scripts/sign-run.sh \
  target/release/examples/metal_handle $L moltenvk 200    # or kosmickrisp
```

The test setup, 2026-11-27:

- **Host:** Apple M3 Max, macOS 26.6.2, load average 5 to 6 (other lanes running).
- **Renderer:** gfxstream from emulator 37.3.10 (`emu -provision-renderer`), and once each on
  the SDK's 37.2.13.
- **Drivers:** MoltenVK (`MoltenVK_icd.json`) and KosmicKrisp (Mesa 26.3.0-devel,
  `libkosmickrisp_icd.json`). Both ship in the renderer's `lib64/vulkan`, so nothing was
  downloaded.
- **Runs:** 3 per driver on 37.3.10 (the numbers below are the medians of the 3 run medians),
  plus 1 per driver on 37.2.13.

## Findings

**The handle (both drivers, both renderer builds).**

- `getColorBufferMetalMemoryHandle` resolves and returns a non-null object for every color buffer
  the renderer created. Two calls return the same pointer.
- The object is an `AGXG15XFamilyHeap`: it conforms to `MTLHeap` and `MTLAllocation`, not to
  `MTLTexture` or `MTLBuffer`.
- It is a placement heap (`MTLHeapTypePlacement`) sized exactly for the image: 3,932,160,
  10,584,064 and 16,384,000 bytes. That is 4 bytes a pixel over the size padded to whole
  64×64-pixel tiles (768×1280, 1088×2432, 2560×1600): a tiled layout, not a linear one.
- **It is not IOSurface-backed.** A heap has no `-iosurface`, so simee can't map it zero-copy;
  every route to an IOSurface is a GPU copy.
- The pointer is unretained and lives as long as the color buffer.

| | MoltenVK | KosmicKrisp |
| --- | --- | --- |
| Heap storage mode | Private (2) | Shared (0) |
| Hazard tracking | Tracked (2) | Untracked (1) |
| Alias matching the readback (both pictures) | usage 0x15, 0x17, 0x11 (all with `PixelFormatView`) | same |
| Alias without `PixelFormatView` (usage 0x5, 0x1) | needs 3,997,696 / 10,846,208 / 16,646,144 bytes, more than the heap: refused | same |
| Blit to RGBA IOSurface | byte-exact | byte-exact |
| Kernel to BGRA IOSurface | byte-exact (red and blue swapped) | byte-exact |

What the sizes imply: both drivers create color-buffer images with
`MTLTextureUsagePixelFormatView`, the mutable-format flag, and Metal keeps such textures
uncompressed. Without that flag a texture of the same size needs a larger allocation, likely
room for lossless-compression metadata, and the heap is too small for it. With the flag the alias
reads the image's bytes exactly, flat picture included.

**Numbers.** Medians in ms per frame. "Process CPU" is the CPU of every thread in the process
for one copy.

| Size | Path | MoltenVK wall | MoltenVK process CPU | KosmicKrisp wall | KosmicKrisp process CPU | GPU |
| --- | --- | --- | --- | --- | --- | --- |
| 1080×2400 | `VkRead` readback (today) | 0.466 | 0.243 | 0.533 | 0.246 | — |
| 1080×2400 | encoder's RGBA→BGRA copy (today, while encoding) | 0.201 | 0.201 | 0.201 | 0.201 | — |
| 1080×2400 | blit to RGBA IOSurface | 0.263 | 0.063 | 0.259 | 0.065 | 0.063 |
| 1080×2400 | kernel to BGRA IOSurface | 0.258 | 0.059 | 0.260 | 0.061 | 0.061 |
| 2560×1600 | `VkRead` readback (today) | 0.566 | 0.333 | 0.591 | 0.351 | — |
| 2560×1600 | encoder's RGBA→BGRA copy (today, while encoding) | 0.328 | 0.328 | 0.318 | 0.318 | — |
| 2560×1600 | blit to RGBA IOSurface | 0.294 | 0.063 | 0.288 | 0.064 | 0.091 |
| 2560×1600 | kernel to BGRA IOSurface | 0.285 | 0.060 | 0.290 | 0.063 | 0.096 |
| 720×1280 | `VkRead` readback | 0.296 | 0.127 | 0.283 | 0.128 | — |
| 720×1280 | kernel to BGRA IOSurface | 0.217 | 0.058 | 0.220 | 0.061 | 0.029 |

More about these numbers:

- **The Metal path's CPU is fixed overhead.** Encoding and committing take 0.008–0.012 ms on the
  calling thread at every size. The rest of its 0.06 ms of process CPU runs on other threads, and
  it doesn't grow with the frame.
- **The readback's CPU grows with the frame.** Most of it is the copy out of the renderer's
  staging buffer.
- **The 37.2.13 renderer gave the same results** within 0.02 ms on both drivers: same handle
  class, same matches.
- **The runs agreed.** The spread across the 3 runs was at most 0.02 ms on any CPU figure.
- **Logs:** `metal_handle` prints every number above; the run logs are
  `~/.claude/orchestrate/kaeawc__simee/mtl356/{mvk,kk}-{1,2,3}.log` and `sdk-{mvk,kk}.log`, not
  in the tree.

## The gate, against the kill criteria

What a Metal present path would save per frame is today's readback, plus the encoder's
RGBA-to-BGRA copy while a stream encodes, minus the kernel into a BGRA IOSurface that the encoder
would wrap without copying (`PixelBuffer::wrap_surface`, as iOS frames are):

| | Phone 1080×2400 | Tablet 2560×1600 |
| --- | --- | --- |
| Today: readback + encoder copy (MoltenVK) | 0.243 + 0.201 = 0.444 ms | 0.333 + 0.328 = 0.661 ms |
| Metal: kernel to BGRA IOSurface | 0.059 ms | 0.060 ms |
| **Saved per frame (MoltenVK / KosmicKrisp)** | **0.385 / 0.386 ms** | **0.601 / 0.606 ms** |
| Saved at 60 fps, share of one core | 2.3% | **3.6%** |
| Kill bar | under 1 ms per frame | under 10% of a core |
| Verdict | under the bar | under the bar |

Both bars are missed by more than 2.5×, so the gate's own rule kills the spike. Without an
encoding stream (the desktop window, screenshots) only the readback goes away: 0.18 ms per frame
on a phone and 0.27 ms on a tablet. A tablet at the 40 fps its stream reached during swipes
(docs/android.md, "Vulkan-only renderer") saves less still.

**What this headless measurement can't see.** In a running guest the GPU is busy, so the
readback's wall time grows; the concept's 0.98 ms phone readback was measured that way. Waiting
for the GPU costs no CPU, though: the readback's CPU is its staging copy. In emu that copy goes
into a ring slot, a disk-backed shared mapping (`<avd>/simee/frames-*.ring`), which may cost more
than a heap buffer, so the per-frame CPU in situ could be somewhat higher than measured here. The
tablet bar is the one with room: it would take about three times the measured CPU (1.67 ms per
frame at 60 fps) to reach 10% of a core. The measurement that settles it is #355's emu CPU per
frame with a guest, which this lane didn't run.

## What would also have to be solved (not tested, because the gate kills first)

- **Ordering.** The copy here runs on its own Metal queue with no synchronization against the
  renderer's Vulkan queue. That's fine for buffers nothing draws into, but not for a guest that is
  posting. The fix would need a `MTLSharedEvent` exported from the renderer (`VK_EXT_metal_objects`)
  or a copy issued from the renderer's own queue. KosmicKrisp's heap is untracked, so it gets no
  automatic hazard tracking at all.
- **Lifetime.** The handle is unretained. A color buffer released (`RefCountPipe`) during a copy
  would free its heap underneath the copy. The handle would have to be retained, with the release
  ordered after the copy completes.
- **Aliasing.** Reading one texture's memory through another texture is outside Metal's
  documented guarantees. It works here because the descriptor matches what the driver created,
  but it would have to be rechecked on each GPU family and macOS release.
- **The post.** emu frames are the compositor's output color buffer
  (`FrameBuffer::getLastPostedColorBuffer`). That is a color buffer like these, so the handle
  applies to it, but this spike doesn't drive a compositor post.

## Side findings

- **`renderer_caps -emu` and `-vulkan-only` crash on this host.** They hit a SIGSEGV in
  `EglOsEglDisplay::EglOsEglDisplay` because gfxstream `dlopen`s `libEGL.dylib` and
  `libGLESv2.dylib` by bare name. Example binaries have no rpath to `lib64/gles_angle`; `emu`
  gets one from `apps/emu/build.rs`. `DYLD_FALLBACK_LIBRARY_PATH=$L/gles_angle:/usr/lib` works
  around it, as the `crates/emu-vmm/src/machine.rs` notes do.
- **On this host gfxstream picks external-memory mode `Metal` on MoltenVK and on KosmicKrisp.**
  The mode can be overridden with the `VulkanExternalMemoryMode` feature string. The modes are
  `OpaqueFd`, `Metal`, `HostAllocation` and others; `ExternalMemory::calculateMode` tries Metal
  first, then OpaqueFd.
- **The same lookup reaches `VkEmulation::getBufferMetalMemoryHandle`**, the buffer counterpart.
  It wasn't exercised.
