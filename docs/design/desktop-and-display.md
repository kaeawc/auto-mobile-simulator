# Desktop app and display pipeline

The design for the display, input and debugging side of emu-rs, written before emu-rs joined
sim-rs. It now describes the Android half of sim-rs's one window (`desktop/`): the window is a
separate process from each `emu`, frames arrive over `<avd>/emu-rs/display.sock` (the protocol is
`sim_core::stream`), and input goes back as stream lines or console commands. What is built and
what isn't: zero-copy IOSurface frames (step 2 below) and the frame HUD are not yet; the socket
stream with downscaling, touch, keys, the rotary wheel and the console pane are. Rune is pinned at
`7a70745`, the same rev sim-rs uses.

## Display: zero-copy, double- or triple-buffered

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
  - Events go to a virtio-input multi-touch device (`ABS_MT_*`) as they arrive. Unlike sim-rs's
    HID path, moves aren't coalesced at 16 ms.
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
