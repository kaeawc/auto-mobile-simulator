# Form factors

Every device has a platform (`android` or `ios`), a form factor and the sub-platform that pairs
them. The model is `sim_core::form_factor`; `sim-rs form-factors` and the API's `form_factors`
print it.

## Support

| Form factor | Android | iOS (Apple) |
| --- | --- | --- |
| `phone` | Android | iOS |
| `tablet` | Android | iPadOS |
| `foldable` | Android | **unsupported**: no Apple device or simulator runtime folds |
| `watch` | Wear OS | watchOS |
| `tv` | Android TV | tvOS |
| `auto` | Android Automotive | CarPlay |
| `vision` | Android XR | visionOS |

A foldable iPhone is the only unsupported pair. Unsupported pairs are rejected wherever they
appear, with the reason:

```text
$ sim-rs create x "iPhone 17" "iOS 26.5" --form-factor foldable
Error: foldable is not supported on iOS: no Apple device or simulator runtime folds; foldables are Android-only
```

`auto` means different things on each side, and the labels say which: on Apple it is **CarPlay**,
the car display an iPhone simulator projects onto; on Android it is **Android Automotive OS**, the
OS running on the head unit itself (what the emulator's automotive images are). Android Auto, which
projects from a phone, isn't an emulator image.

`vision` is a headset on both sides: **visionOS** on Apple, **Android XR** on Android. Google
publishes arm64 Android XR images that run on Apple silicon, from the `google_xr` repository site
(`sys-img/google_xr/sys-img2-*.xml`, listed in the SDK's site list): `google-xr`,
`android-xr-v3-playstore` and `android-xr-preview-playstore` (headsets, the last also XR glasses;
all carry the `android-xr` tag) at API 34, and `ai-glasses` (display glasses) at API 36. All are
developer previews.

## Reference profiles

What a new device of each kind gets unless told otherwise. Real devices report their own screen.

| Kind | Screen (px) | Density | Shape | Fold | Inputs |
| --- | --- | --- | --- | --- | --- |
| Android phone | 720×1280 | 320 dpi | rounded | — | touch, keyboard, buttons |
| Android tablet | 2560×1600 | 320 dpi | rounded | — | touch, keyboard, buttons |
| Android foldable | 2208×1840 (folded 1080×2092) | 420 dpi | rectangle | hinge at 180° | touch, keyboard, buttons, hinge |
| Wear OS watch | 454×454 | 320 dpi | round | — | touch, rotary input, buttons |
| Android TV | 1920×1080 | 320 dpi | rectangle | — | D-pad remote, keyboard |
| Android Automotive | 1024×768 | 160 dpi | rectangle | — | touch, head unit |
| Android XR headset | 2560×2558 (landscape) | 240 dpi | rectangle | — | eye tracking, hand tracking, keyboard |
| Android XR glasses (`ai-glasses` images) | 450×450 (landscape) | 160 dpi | rectangle | — | eye tracking, hand tracking, keyboard |
| iPhone | 1206×2622 | @3x | rounded | — | touch, keyboard, buttons |
| iPad | 1668×2420 | @2x | rounded | — | touch, keyboard, buttons |
| Apple Watch | 416×496 | @2x | rounded | — | touch, Digital Crown, buttons |
| Apple TV | 1920×1080 | @1x | rectangle | — | Siri Remote, keyboard |
| Apple Vision Pro | 2732×2048 | @2x | rectangle | — | gaze, pinch, keyboard |
| CarPlay | 800×480 | @2x | rectangle | — | touch, head unit |

The Android phone keeps emu-rs's deliberately small default: smaller screens boot markedly faster.
The Android XR profiles are the SDK's own device definitions (`xr_headset_device`, 4 GiB;
`ai_glasses_device`, 2 GiB; both landscape), and a new XR AVD gets their RAM and
`hw.initialOrientation=landscape` too.
Foldable postures follow the stock emulator's hinge ranges: closed below 30°, half-opened below
150°, opened from 150° to 180°.

## How a device's form factor is found

- **iOS**: from the device type identifier (`iPhone-…`, `iPad-…`, `Apple-Watch-…`, `Apple-TV-…`,
  `Apple-Vision-…`), else the runtime (`watchOS-…`, `tvOS-…`, `xrOS-…`). An unrecognized type has
  no form factor and is labelled by platform only. CarPlay is never inferred: it isn't a device
  type.
- **Android**: from the AVD's `config.ini` and image. The image tag decides XR (`android-xr*`,
  `google-xr`, `ai-glasses`), watch (`android-wear`, `android-wear-signed`, `android-wear-cn`), TV
  (`android-tv`, `google-tv`, and 16 KB page-size variants such as `google-tv-ps16k`) and
  automotive (`android-automotive*`); a hinge sensor
  (`hw.sensor.hinge=yes`) or a `fold` device name makes a foldable; a smallest width of 600 dp or
  more (Android's own threshold) makes a tablet; otherwise a phone. A Wear device name with
  `square` or `rect` gets a rectangular screen, else round.

## Launching each form factor

| Kind | Create | Drive |
| --- | --- | --- |
| iPhone | `sim-rs create P "iPhone 17" "iOS 26.5"` | touch, `button home\|lock`, `type` |
| iPad | `sim-rs create T "iPad Pro 11-inch (M4)" "iOS 26.5" --form-factor tablet` | as iPhone |
| Apple Watch | `sim-rs create W "Apple Watch Series 10 (46mm)" "watchOS 26.0" --form-factor watch` | touch, `button home` (unverified, see below) |
| Apple TV | `sim-rs create TV "Apple TV 4K (3rd generation)" "tvOS 26.0" --form-factor tv` | `key` (arrows, Return; unverified, see below) |
| Vision Pro | `sim-rs create V "Apple Vision Pro" "visionOS 26.0" --form-factor vision` | touch (unverified, see below) |
| Android phone | `sim-rs create P --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"` | touch, `button back\|home\|recents`, `type`, `rotate` |
| Android tablet | the same image, `--form-factor tablet` | as phone |
| Android foldable | the same image, `--form-factor foldable` | as phone, plus `fold <closed\|half-opened\|opened\|angle>` |
| Wear OS | `--system-image "system-images;android-34;android-wear;arm64-v8a"` | touch, `rotary <ticks>`, `button back\|home\|power` |
| Android TV | `--system-image "system-images;android-34;google-tv;arm64-v8a"` | `button up\|down\|left\|right\|select\|back\|home` |
| Automotive | `--system-image "system-images;android-34;android-automotive;arm64-v8a"` | touch, `button back\|home` |
| Android XR headset | `--system-image "system-images;android-34;google-xr;arm64-v8a"` (install: `emu -provision 34 -tag google-xr`) | `xr recenter`, `xr rotate X Y Z` (radians), `xr move DX DY DZ` (metres), `xr options day\|night PASSTHROUGH`, `xr input mouse\|hand\|eye`, `xr viewport pan\|zoom\|rotate`, `xr pointer X Y [move\|down\|up\|click]`, `type` |
| Android XR glasses | `--system-image "system-images;android-36;ai-glasses;arm64-v8a"` | as the headset (unverified, see below) |

The device type and runtime names above are examples: `sim-rs device-types`, `sim-rs runtimes` and
`emu -list-images -tag <tag>` list what your host has (`-tag android-xr` lists the XR headsets by
the tag they carry; `-provision` takes the tag in the package path, `google-xr`).

`sim-rs xr DEVICE ...` (the API's `xr` method) goes through emu-rs's `xr` console command, which
sends the guest what the stock emulator's XR controls send over the `qemud:xr_service` pipe
(stock only offers them over gRPC), and `xr pointer` drives the virtio dual-mode mouse emu-rs adds
to XR images. `adb emu xr status` says whether the guest's XR service is connected and what it
last answered. The desktop gives XR devices look-left, recenter and look-right buttons. `--form-factor` on iOS only checks the device
type; on Android it also shapes the new AVD (screen size, a foldable's hinge sensor). An image
can't boot another family's form factor:

```text
$ sim-rs create T --platform android --system-image "system-images;android-34;android-wear;arm64-v8a" --form-factor tablet
Error: system image `system-images;android-34;android-wear;arm64-v8a` (tag `android-wear`) boots Wear OS · Watch devices, not tablet
```

## Known gaps

What is modelled and labelled but not (or not verifiably) working end to end:

| Kind | Gap |
| --- | --- |
| CarPlay | Attaching the CarPlay display to an iPhone simulator isn't implemented; `create --form-factor auto` on iOS says so. |
| Android Automotive | emu-rs doesn't implement the vehicle HAL (`qemud:car`) or its audio HAL yet, so automotive images may not reach the launcher ([android.md](android.md)). |
| Android TV | Not among the images emu-rs has booted so far. |
| Android XR | Not booted on emu-rs yet. The images expect a sound card (stock enables `VirtioSndCard` for them); virtio-snd is being added to emu-rs. The room renders in the guest through gfxstream (emu-rs already defaults to ANGLE), but stock's other XR feature flags (`XrModeUI`, `XrHandAndEyePointers`, `VirtioMouse`, `MultiDisplay`, `BluetoothEmulation`) have no emu-rs equivalent yet. The dual-mode mouse matches stock's device (name, ids, event bits, axes); how the guest maps it to its hand ray and gaze pointers, and whether head pose and options over `xr_service` take effect, is unverified. `emu -create-avd` doesn't apply the XR profile (pass `-lcd 2560x2558@240 -memory 4096`); `sim-rs create` does. |
| Apple Watch, Apple TV, Vision Pro input | The Digital Crown, Siri Remote and gaze/pinch aren't injected yet; sim-rs sends these devices the same touch and HID key messages as an iPhone, and whether each runtime honours them hasn't been checked. The API's `rotary` is Android-only. |
| iOS rotation | `rotate` is Android-only. |
| Every Apple form factor | Needs a Mac with the runtime installed; none could be booted in the environment the combined project was built in. |
