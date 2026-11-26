# Desktop app and display pipeline

The design for the display, input and debugging side of emu-rs, written before emu-rs joined
sim-rs. It now describes the Android half of sim-rs's one window (`desktop/`): the window is a
separate process from each `emu`, frames arrive over `<avd>/simee/display.sock` (the protocol is
`sim_core::stream`) or through a shared frame ring it announces, and input goes back as stream
lines or console commands. What is built and what isn't: the frame path below (one readback per
frame, a shared-memory ring, downscaling), touch, keys, the rotary wheel and the console pane are;
zero-copy IOSurface frames (step 2 of the plan after it) and the frame HUD are not yet. Rune is
pinned at `7a70745`, the same rev sim-rs uses.

## Frame path (built)

1. **Post.** Host composition posts a color buffer; the renderer's post callback only records the
   frame's size and number (`DisplaySink::posted`). Its pixels aren't copied: the renderer's GL
   readback that feeds them is stale under Vulkan composition (and not even filled with async
   readback). They're copied only when the renderer can't be read directly.
2. **Readback, once per frame.** `FrameStore::latest` reads the posted color buffer back when a
   stream or screenshot first wants that frame and keeps it as RGBA, so streams and screenshots of
   one frame share one readback. The renderer reads the Vulkan image directly
   (`VkEmulation::readColorBufferToBytes`, a private symbol found through the backend's symbol
   table, checked against the exported GL path on the first frames) instead of copying it to GL and
   reading GL back under the FrameBuffer lock. A full-size stream skips the capture and reads back
   straight into its ring slot (`FrameStore::read_into`).
3. **Scale.** `sim_core::pixels::downscale_into` box-filters straight into the destination,
   in vectorizable row passes, in bands across up to 4 threads for large frames.
4. **Transport.** A client that sends `shm` gets a ring file (`<avd>/simee/frames-<pid>-<n>.ring`,
   3 page-aligned slots, a seqlock per slot): each frame is written into a slot and announced with
   a 32-byte `EMUS` record; the client `pread`s the slot (no mapping, no `unsafe` on its side) and
   checks the slot's sequence number; one that can't open the ring sends `socket` and gets socket
   frames. A ring is announced with the first frame sent from it, written out in full when made,
   and removed with its stream or when the emulator exits. Other clients get `EMUF` frames on the
   socket as before.
5. **Pacing.** At most one frame per interval in the long run, kept as a GCRA (a token bucket
   held as one instant: the next frame's due time, advanced by an interval from when the last
   was due or taken, whichever is later; a frame may go up to half an interval early). A frame
   posted after a quiet spell goes out at once; an early one waits and the newest by then goes.
   The interval is 16 ms: a steady 60 Hz stream goes out frame for frame (`frame_path_latency`,
   2480x1740 into `hello 1080` by ring, posts every 16 ms: 120/120 delivered, post -> client
   0.9 ms median, 3–4 ms max; with the old 33 ms gap measured from the end of each send: 49/120,
   5.5–10.3 ms median, 20–42 ms max). A client may send an optional `fps <n>` line (1–240) for
   its own cap; servers that don't know it ignore it, so it's compatible both ways. A socket
   client that stops reading paces itself (the write blocks); a ring client reads a slot within
   two intervals or sees it torn and skips it.
6. **Window.** The desktop reads each frame into the buffer it hands to `Pixels`.
7. **Screenshots.** The console's `screenshot` encodes the shared capture directly.

Guest-composed scanouts (virtio-gpu `RESOURCE_FLUSH`) are read straight into the store
(`DisplaySink::present_with`). Numbers and what's left: [android.md](../android.md#performance-notes).

**What zero-copy still needs.** gfxstream allocates color buffers in MoltenVK `MTLHeap`s, which
can't cross processes, and exports neither an IOSurface nor its Metal objects
(`exportColorBuffer` returns nothing in Metal mode). Showing frames without a CPU copy needs either
a renderer that creates its display color buffers on IOSurfaces (`VK_EXT_metal_objects`'
`VkImportMetalIOSurfaceInfoEXT`) or a GPU blit, on MoltenVK's queue, from the posted image into an
IOSurface ring; then the surfaces' Mach ports to the window (a bootstrap rendezvous, as Chromium
does) and `PixelBuffer::from_iosurface` there (BGRA). The ring protocol stays the fallback and the
Linux path.

## Display: zero-copy, double- or triple-buffered (planned)

1. **Scanout.** virtio-gpu scanout (or the gfxstream composer's output) renders into IOSurfaces
   in the VMM process, two or three of them in a ring.
2. **Push.** When the guest flushes a scanout, the VMM wraps the finished surface once with
   `rune::native_surface::macos::PixelBuffer::from_iosurface(raw, ColorInfo::default())`
   (BGRA8, 420v or 420f). It pushes that buffer with `NativeSurface::push(buf.clone(), frame_id)`
   and moves on to the next surface in the ring.
   - Rune requires a surface to stay immutable while the GPU samples it. sim-rs breaks that by
     reusing one surface and accepts tearing; a ring meets the requirement.
3. **Present.** The view shows the frame with
   `el().key("device-screen").aspect_fit(w / h).native_surface(&ns, SurfaceFit::Fill)`.
   - `NativeSurface::set_waker` wakes the UI only when a frame arrives.
   - Pacing follows the display link, and an idle guest costs nothing.
4. **Headless scenes and CI** use `El::pixels(&Pixels)` with `Pixels::set(w, h, rgba)` instead.
5. **Where the UI lives.** The desktop app and the VMM are separate processes; a crashed guest
   shouldn't take the window down. IOSurfaces cross process boundaries by Mach port
   (`IOSurfaceCreateMachPort` / `IOSurfaceLookupFromMachPort`), so zero-copy still holds. An
   in-process mode (desktop links `emu-vmm`) is the fallback if port passing costs latency.
6. **Rotation.** Rune can't rotate a pushed surface (`push_oriented` is crate-private). The guest
   rotates its own output, the way stock does; the window refits with `tx.resize_window`.

## Input

- **Touch.** `on_drag` delivers `Drag { phase, x, y, w, h }` in the element's own layout points.
  Guest pixel = `x / w * guest_w`, so no HiDPI code is needed.
  - Events go to a virtio-input multi-touch device (`ABS_MT_*`, protocol B) as they arrive.
    Unlike sim-rs's HID path, moves aren't coalesced at 16 ms.
  - The device is stock's (`virtio_input_multi_touch_1`): guest pixels are scaled to its
    0..32767 axes in emu, so every caller (this window, the console, `display.sock`) speaks
    pixels. A down sends what stock's mouse down does (slot, tracking id, position, full
    pressure, touch size); a move sends only the position that changed and `SYN` (2–3 events),
    as stock's `_mts_pointer_move` does; a lift zeroes the pressure and ends the tracking id. The
    kernel keeps each slot's values and drops repeats, so the guest's readers see the same
    events. A raw `event send` on the touch device makes the next touch send the full set again.
  - Moves the guest hasn't started taking collapse into the newest (a stalled guest gets the
    current position, not a backlog); downs, ups and keys are never dropped or reordered. Events
    that arrive together go out with one interrupt.
  - `d.cancelled()` sends a real cancel.
- **Pinch.** `on_magnify` drives two synthetic contacts.
- **Scroll.** `on_wheel` (`WheelEvent { dx, dy, precise, momentum }`) goes to `REL_WHEEL`/`REL_HWHEEL`;
  on Wear images it goes to the rotary encoder.
- **Keyboard.** `on_key` on the focused screen element maps to Linux keycodes on a virtio-input
  keyboard.
  - Shortcuts: ⌘H home, ⌘B back, ⌘P power, ⌘S screenshot, ⌘R rotate.
  - Returning `false` passes the key on to the window.
  - Button holds release on a later frame through `tx.wake_after`.

## One control surface, many front ends

Every capability is added once, to a method table in the VMM (the sim-api pattern), and served
over:

- the emulator console port, which keeps the stock text protocol for `adb emu` and harnesses;
- a JSON-lines unix socket for scripts and the desktop app;
- the `emu` CLI (`emu call <method> '<json>'`).

The desktop's debug panel is a client of the same socket, so anything it can do, a script can
too.

## Debugging tools

These are things sim-rs lacks and emu-rs should have from the start:

- **Frame HUD.** Built on `NativeSurface::set_observer`
  (`FrameTiming { submitted_at, completed_at, gpu_start, gpu_end }`) and `counts()`. Shows
  guest→present latency, fps and dropped frames.
- **VMM counters.** Per-vCPU exit counts (`-verbose` already collects them), IRQs/s, disk and
  vsock bytes/s, guest RSS against the configured RAM.
- **Panes:**
  - guest console (done);
  - logcat over virtio-console `hvc1`;
  - processes and memory (`dumpsys meminfo`);
  - network shaping (console `network speed|delay`).
- **Headless.** `--snap`, scenes and `Runtime::find`/`presented` tests for every control. Include
  a `drags_map_to_device_pixels` test like sim-rs's.

## Harness loop

`scripts/automobile-harness.sh` runs AutoMobile from source against a private daemon (copied from
sim-rs) and adds a per-tool pass/fail matrix (`coverage.tsv`). It can attach to a running emu-rs
(`emulator-5554`) or let AutoMobile boot an AVD through the SDK overlay (`@avd`). The loop for
every fix is:

1. `cargo c && cargo lint && cargo t`
2. Boot.
3. Run the harness.
4. Extend the `tools` list when a tool starts passing.
