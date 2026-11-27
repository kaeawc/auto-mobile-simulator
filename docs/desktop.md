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

The window draws with Rune's native Metal backend. `SIMEE_GPU_BACKEND=wgpu` switches it to wgpu's
Metal backend, which the 3D device view needs, and `metal` chooses the default. The window looks
and behaves the same on both, but wgpu costs a little more:
[benchmarks.md](benchmarks.md#desktop-runes-native-metal-backend-against-wgpu). Rune's own
`RUNE_GPU_BACKEND` overrides it.

## 3D device view

⌘3 switches the screen to a 3D view: the device's body (`sim_core::body`: phones, tablets,
foldables and watches) in perspective, lit, with its live screen on the screen face. It is drawn
with a depth buffer and 4x MSAA. `SIMEE_VIEW=3d` starts the window with it on. The 3D view needs
the wgpu backend (`SIMEE_GPU_BACKEND=wgpu`). Rune's native Metal backend can't draw a depth
buffer or MSAA yet (anomalyco/rune#2547, #193), so on it ⌘3 only says so in the status line. A TV,
head unit or headset has no body and stays flat. With the 3D view off, the window is exactly the
flat one: screenshots are byte-identical to the build without it on both backends.

The camera is fixed: a 3/4 view from the right and above. `SIMEE_SCENE_CAMERA=front` or `side`
picks the straight-on view or the edge-on view (thickness, keys, camera bump) for screenshots.
Orbit, pan and zoom come with #155. When a screen is on the back of the device (a folded book's
cover screen), the camera looks at the back. A turned device rolls the body with its panel, so
the screen reads as the flat view shows it. A foldable lies flat while it streams its inner
screen. When it streams its cover screen, emu has folded it, so the body is shown shut. The
desktop doesn't know a half-open angle (#154). Clicks on the 3D view don't reach the device yet
(#156): ⌘3 says so in the status line. The toolbar, the keyboard and the panel work as usual,
and clicking the 3D view keeps the keyboard on the device. `RUNE_GPU_BACKEND` is read as Rune
reads it (exactly `wgpu` or `metal`). Turning the 3D view off frees its render targets.

The live screen on the body is the flat view's own picture, with no CPU copy. The flat screen
stays laid out under an effect slot that asks Rune for a backdrop copy, and the compositor
samples the screen's rect of that copy. That costs one GPU texture copy of the window per drawn
frame. Rune can't yet hand a compositor a `NativeSurface`'s texture (anomalyco/rune#2546,
#192). The window draws only when something changes, so a still 3D view costs nothing.

What the 3D view costs, against the flat view on the same wgpu backend (M3 Max, 2026-11-27):

| | flat | 3D |
| --- | --- | --- |
| Headless, iPhone 17 scene (`--frames 300`, median of 3): frame CPU p50 | 0.25 ms | 0.43 ms |
| Headless, iPhone 17 scene: encode and submit | 0.16 ms | 0.32 ms |
| Headless, iPhone 17 scene: GPU per frame (includes the frame readback) | 1.56 ms | 1.81 ms |
| Headless, Pixel Fold scene: frame CPU p50 / GPU per frame | 0.22 / 1.59 ms | 0.40 / 1.90 ms |
| Live iPhone 17 Pro, iOS 26.5, 20 s of swipes (one run each): CPU / GPU, % of a core | 6.0 / 5.5 | 8.3 / 6.3 |
| Live, same run: frames drawn, frame time p50 / p95 | 775, 0.54 / 0.62 ms | 786, 0.79 / 0.94 ms |
| Live, idle 20 s: CPU / GPU % | 1.25 / 0.65 | 1.40 / 0.68 |

The live rows are a single run each, taken as #215 took them (`--window-script`, `RUNE_FRAME_LOG`,
the AGX client's GPU time). A second run of each stopped drawing because its window was covered
on a desktop in use (Rune draws nothing while a window is occluded), so there is no median yet.
The headless rows render offscreen and don't depend on window visibility.

Rune's headless flags (`--snap`, `--tree`, `--script`, `--video`) are always in debug builds; a
release build has them only with the `tooling` feature. The shipped release build leaves it off:
it adds 1,338,496 B to the stripped binary (32,950,904 → 34,289,400 B, Linux x86_64, 2026-11-26)
and no measurable startup time.

Devices are listed under iOS and Android, each with its sub-platform. Pick one (boot it if
needed): click and drag on the screen for touches; type to use the device keyboard. ⌘⇧H is Home,
⌘L Lock/Power, ⌘S saves a screenshot to the Desktop, ⌘3 switches the [3D view](#3d-device-view)
on and off. The toolbar follows the form factor:

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

