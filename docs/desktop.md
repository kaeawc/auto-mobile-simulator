# Desktop window

One window for iOS and Android devices.

![The desktop window with iOS and Android devices](desktop.png)

```bash
cd desktop
cargo run --release                     # SIMEE_DEVICE=<udid|name|booted> preselects a device
cargo run -- --snap shot.png --scene demo   # headless render; scenes: demo, android, foldable,
                                            # folded, tablet, ipad, watch, tv, apple-watch, apple-tv, empty
cargo run --release --features tooling -- --snap shot.png --scene demo   # the same, optimized
```

Rune's headless flags (`--snap`, `--tree`, `--script`, `--video`) are always in debug builds; a
release build has them only with the `tooling` feature. The shipped release build leaves it off:
it adds 1,338,496 B to the stripped binary (32,950,904 → 34,289,400 B, Linux x86_64, 2026-11-26)
and no measurable startup time.

Devices are listed under iOS and Android, each with its sub-platform. Pick one (boot it if
needed): click and drag on the screen for touches; type to use the device keyboard. ⌘⇧H is Home,
⌘L Lock/Power, ⌘S saves a screenshot to the Desktop. The toolbar follows the form factor:

| Device | Controls |
| --- | --- |
| iPhone / iPad | Home, Lock, Rotate, Appearance |
| Apple Watch | Crown press, Side button, Crown up / down |
| Apple TV | Menu, TV, arrows and Select (the Siri Remote), Appearance |
| Apple Vision Pro | Home, Appearance |
| Android phone | Back, Home, Recents, Power, Rotate |
| Android tablet | the above, plus Stylus (the pointer becomes a pen: moving over the screen hovers, dragging draws) |
| Android foldable | the above, plus Fold / Half open / Unfold (folded, the view takes the outer screen's shape) |
| Wear OS watch (round screen) | Back, Home, Power, Rotary up / down |
| Android TV | Back, Home, Power, D-pad and Select |
| Android Automotive | Back, Home |
| Android XR | Back, Home, Power, Look left / Recenter / Look right |

iOS frames go to the GPU without copying (the framebuffer IOSurface is sampled directly) and the
right-hand panel has five debug tabs (Device, Network, Storage, Processes, Logs). Android frames
stream from the emulator's display socket, and the panel shows the device and its live guest
console.

