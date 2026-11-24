# Simulator audio and camera: research

How a simulated iOS device's speaker, microphone and camera reach the host, what simee can
control without root, a host audio driver or an MDM profile, and the API this suggests for simee and
auto-mobile.

- Host: macOS 26.6.2 (25G83), Xcode 26.6 (17F113), CoreSimulator 1174.21, Apple silicon.
- Device: `simee-test` (D99E12ED-…), iOS 26.5 runtime (23F77), booted headless by simee, with no
  Simulator.app running.
- Host audio devices: MacBook Pro Microphone (`BuiltInMicrophoneDevice`), MacBook Pro Speakers
  (`BuiltInSpeakerDevice`) and BlackHole 2ch (`BlackHole2ch_UID`). BlackHole was already installed.
  It was used only to observe what the guest sends and receives; none of the driverless results
  depend on it.
- The probes are in `scratch/audio-camera/` (untracked): `dump.swift`, `route.swift`,
  `hostaudio.swift`, `canbedefault.swift`, `vol.swift`, `simaudio.m` and `camprobe.m`. Each probe's
  header comment gives its build command.
- Each finding is tagged **[verified]** (seen in an experiment on this host) or **[inferred]**
  (taken from symbols, strings or logs without a direct experiment).

## Summary

| Question | Answer |
| --- | --- |
| Where does device audio go? | Each in-device process is itself a client of the host's `coreaudiod`. It builds a private per-process aggregate, `iOSSimulatorAudioDevice-<pid>-N`, from the host device UIDs in its settings plist. No CoreSimulator process carries samples. **[verified]** |
| Can simee be the device's audio endpoint (receive output, supply mic samples) through a private API? | No. No I/O port or SimulatorKit class carries samples. The only audio port (`com.apple.CoreSimulator.Audio.HostRoute`) selects host device UIDs. **[verified port list; inferred: no hidden sample path]** |
| Driverless output capture | Yes. A Core Audio **process tap** (public, macOS 14.2+) on the device's PIDs captures its output exactly, with no driver. **[verified]** |
| Driverless per-device "null sink" | Yes, three ways: a muted process tap, `sim_volume = 0`, or pinning output to a silent device. **[verified]** |
| Driverless microphone injection | No. Pointing the guest at a tap-backed aggregate fails (`AudioQueueStart` -66628), and the router hides such devices. Mic injection needs a real loopback device (driver) or in-app hooking. **[verified failure]** |
| Per-device pinning to a device the router refuses (BlackHole 2ch) | Works by writing the guest's `audiosettings.plist`, which bypasses the router. Applies to audio clients that start after the write. **[verified]** |
| Camera | iOS 26.5 exposes no video capture device. CMCapture has only stub `FigCaptureSourceSimulator` / `FigCaptureSessionSimulator`, and no CoreSimulator port or string mentions a camera. **[verified]** Injecting auto-mobile's poster dylib through simee `launch` env works. **[verified]** |
| Device-wide injection | Boot environment (`simee boot --env DYLD_INSERT_LIBRARIES=…`, or `network_shape {inject:"boot"}`): every process loads the dylib, including WebKit's ExtensionKit processes **[verified, network-shaping test]**. `launchctl setenv`: observed on iOS 26.5 (network-shaping test, see `docs/research/network-shaping.md`): after `launchctl setenv DYLD_INSERT_LIBRARIES <dylib>` inside the device, processes launchd starts afterwards load the dylib (MobileSafari relaunched, `managedappdistributiond`, `mlhostd`). Already-running processes do not until restarted. ExtensionKit extensions (e.g. `com.apple.WebKit.Networking`, `WebContent`) get a sanitized environment and never load it. `launchctl unsetenv` restores the device. |

## 1. Audio output

### Path **[verified]**

1. The runtime's `profile.plist` lists `com.apple.audio.audiohald` and `com.apple.audio.coreaudiod`
   under `requiredHostServices`, so `launchd_sim` forwards the host's Core Audio server into the
   device.
2. The guest's CoreAudio is the full HAL client (`AudioHAL_Sim` sources). The guest AudioToolbox
   (`AggregateDevice.mm`, "Creating DefaultDeviceAggregate") reads
   `SIMULATOR_AUDIO_SETTINGS_PATH`, i.e. `<device>/data/var/run/simulatoraudio/audiosettings.plist`
   (`sim_input_device_uid`, `sim_output_device_uid`, `sim_volume`, `sim_ringer_state`). It then
   creates a private host aggregate, `iOSSimulatorAudioDevice-<pid>-N`, over those devices. The
   device log shows `Built valid aggregate 153`, and coreaudiod shows
   `IOWorkLoopInit: … BuiltInSpeakerDevice (iOSSimulatorAudioDevice-78215-0): starting`.
3. Proof: I ran `simaudio tone` inside the device (`xcrun simctl spawn`). The host's
   `kAudioHardwarePropertyProcessObjectList` then listed that exact PID (parent: the device's
   `launchd_sim`) with `IsRunningOutput=1` on `BuiltInSpeakerDevice`. SpringBoard and
   `systemsoundserver-simd` from every booted device also appear as host Core Audio clients.
4. `SimAudioProcessorService` (one XPC service per user, in
   `CoreSimulator.framework/Resources/SimAudioProcessorServices.simdeviceio`) only manages routing.
   It watches host device changes, resolves the selected UIDs and writes the two plists. It is never
   in the sample path: during playback it was idle, with `runIn=0 runOut=0`.

So with no Simulator.app, a headless device's output **plays on the host**: on the host default
output, or on whichever device the plists name. `sim_volume` is applied inside the guest, so the
host device volume is unchanged. Measured: 60 gives 0.36× amplitude, 100 gives unity and 0 gives
silence **[verified]**. **Not reproduced later** (simee `audio_volume`): writing `sim_volume` 60 and 100
then playing the same tone through a device pinned to BlackHole gave peak 0.300 both times, and a
process tap also saw full amplitude. Treat volume as unverified.

### Device I/O ports (iOS 26.5, from `dump.swift`) **[verified]**

`com.apple.display.captureservice` (SimScreenCaptureService), `com.apple.framebuffer.display` ×3,
`com.apple.framebuffer.server`, `com.apple.accelerator.metal`, `com.apple.accelerator.iosurface`,
`com.apple.CoreSimulator.HID.LegacyHID`, `com.apple.SimStreamProcessor.default` ×3,
**`com.apple.CoreSimulator.Audio.HostRoute`**, `com.apple.gputools.service.simulator` and
`com.apple.gputools.GPUToolsAgentService.simulator`. There is no camera port, no audio sample port
and no microphone port.

The audio port's descriptor proxy conforms to `SimDeviceIOPortDescriptorInterface`,
`SimAudioHostRoutable` and `SimEnvironmentProvider` (it injects the two `SIMULATOR_AUDIO_*` env
vars).

`SimAudioHostRoutable`, a protocol in CoreSimulator, reached via `SimDevice.io.ioPorts[i].descriptor`:

| Selector | Type encoding |
| --- | --- |
| `availableHostDevices` | `@16@0:8` (NSArray of `SimAudioHostDevice` proxies) |
| `guestInputHostDeviceUID` / `guestOutputHostDeviceUID` | `@16@0:8`; `__sim__hostUseSystemDefaultDeviceUID` means "follow host default" |
| `effectiveDefaultInputDeviceUID` / `effectiveDefaultOutputDeviceUID` | `@16@0:8` |
| `routeGuestDeviceScope:toHostDeviceUID:completionQueue:completionHandler:` | `v48@0:8Q16@24@32@?40` (scope 1 input, 2 output; block `(NSError*)`) |
| `registerCallbackWithUUID:selectedDeviceUIDChangedCallback:` | `v32@0:8@16@?24` |
| `registerCallbackWithUUID:availableDevicesChangedCallback:` | `v32@0:8@16@?24` |
| `unregisterCallbackWithUUID:` | `v24@0:8@16` |
| `uuid` | `@16@0:8` |

`SimAudioHostDevice` (each `availableHostDevices` item, a `ROCKImmutableProxy`):
`uniqueIdentifier`, `name`, `displayName`, `dataSourceName` (`@`), `dataSourceID` (`I`), `scope`
and `isDefaultInScopes` (`Q`: 1 input, 2 output, 3 both), `inputChannels` and `outputChannels`
(`q`).

### SimDeviceAudioClient (SimulatorKit) **[verified methods; inferred usage]**

- The runtime class name is `_TtC12SimulatorKit20SimDeviceAudioClient`, resolved with
  `NSClassFromString`. `nm` shows an `OBJC_CLASS_$_SimDeviceAudioClient` symbol, but the plain name
  does not resolve at runtime.
- Methods: `-initWithDevice:eventHandlerQueue:error:changedEventHandler:` (`@48@0:8@16@24^@32@?40`),
  `-currentState` (`@16@0:8`) and `-routeGuestWithScope:to:completionQueue:completionHandler:`
  (`v48@0:8Q16@24@32@?40`).
- `_TtC12SimulatorKit25SimDeviceAudioClientState` has `inputDevices`, `outputDevices`,
  `selectedInputDevice`, `selectedOutputDevice`, `effectiveDefaultInputDevice`,
  `effectiveDefaultOutputDevice` (`@`) and `useDefaultInputDevice`, `useDefaultOutputDevice`
  (`B`), each with a setter.
- It is a convenience wrapper over the same `SimAudioHostRoutable` port. Simulator.app holds one per
  device in `DeviceCoordinator.audioClient`. It builds the I/O > Audio Input / Audio Output menus
  from `currentState` (`updateAudioMenuWithState:`) and calls
  `_onQueue_synchronizeSelectedAudioDevices:hostDevices:currentlySelectedDevice:currentlyUsingDefaultDevice:inScope:`
  with its `RecentAudioInputDeviceUIDs` / `RecentAudioOutputDeviceUIDs` preferences. It also has
  `audioVolumeUp:` / `audioVolumeDown:`, which presumably drive `sim_volume`.
- simee does not need SimulatorKit: the port protocol does everything the client does, and it is
  in CoreSimulator, which simee already loads.

### Routing behavior on this host **[verified]**

- `routeGuestDeviceScope:1 toHostDeviceUID:"__sim__hostUseSystemDefaultDeviceUID"` **restores
  "use host default"**. The read-back returns the sentinel and the plist's
  `useHostDefaultInputDevice` goes back to `true`. This answers auto-mobile #13631's open question
  ("a nil UID does not restore default").
- A route made by one process was still in effect when a second process read it, so the pin
  outlives the client on CoreSimulator 1174.21.
- The router silently ignores UIDs it does not list. A route to `simee-loopback` (my aggregate)
  completed with `error=nil`, but it pinned the effective default (`BuiltInMicrophoneDevice`)
  instead.
- `availableHostDevices` omitted a public aggregate that a plain process could see. The
  aggregate's `kAudioDevicePropertyDeviceCanBeDefaultDevice` is 0 in both scopes, while the built-ins
  and BlackHole report 1. The service binary contains the `'dflt'` / `'inpt'` / `'outp'` property
  fourCCs, so the router probably filters on CanBeDefault **[inferred]**. BlackHole (scope 3) is
  listed, but auto-mobile measured that routes to it are dropped.
- **Bypass:** writing `sim_input_device_uid` / `sim_output_device_uid` in
  `simulatoraudio/audiosettings.plist` while the device is booted took effect for the next audio
  client that started:
  - Input pinned to BlackHole: the in-device recorder heard a host 660 Hz sine played into BlackHole
    at RMS 0.212 (0.3/√2).
  - Output pinned to BlackHole: the host read the in-device 440 Hz tone back.

  Editing `com.apple.coresimulator.audio.plist` alone did nothing; the guest reads
  `audiosettings.plist`. The service rewrites the file at boot and on route or host device
  changes, so an owner has to re-apply it. Whether running clients pick up a change, for example via
  the `com.apple.CoreSimulator.audio.devices_changed` notification, was not tested.

### Driverless output capture with process taps **[verified]**

`hostaudio tap <pid> <secs> mute`:
1. `kAudioHardwarePropertyTranslatePIDToProcessObject` maps the in-device PID to a process object.
2. `CATapDescription(stereoMixdownOfProcesses:)` with `privateTap = true` and
   `muteBehavior = .muted` describes the tap.
3. `AudioHardwareCreateProcessTap` creates it, and a private aggregate with
   `kAudioAggregateDeviceTapListKey` carries it.
4. An `AudioDeviceIOProc` reads from that aggregate.

Results:
- Device tone at amplitude 0.004 → tap peak 0.0040, RMS 0.00282 (0.004/√2). The tap is exact and
  taken before `sim_volume`.
- With the guest output pinned to BlackHole and a **muted** tap on the guest PID, BlackHole read
  0.0000 while the tap read peak 0.25. Muting gives a per-device null sink while still capturing.
  When the tap was destroyed, BlackHole immediately read the tone again (peak 0.09), so the mute
  lasts only as long as the tap.
- No driver, no root, no default-device change. The capture is per process, so two devices never
  mix.

Each in-device process is a separate tap source. Enumerate the device's processes (children of its
`launchd_sim`, which sim-debug already walks) that appear in `kAudioHardwarePropertyProcessObjectList`.
Watch that property for new clients, and update `kAudioTapPropertyDescription`, which is settable,
to add them. `CATapDescription.bundleIDs` (macOS 26) cannot tell two devices apart.

TCC: tap creation made tccd checks attributed to the responsible app of the tapping process (here
`com.anthropic.claude-code`). No prompt appeared and real samples arrived, so that app already had,
or did not need, the "System Audio Recording" permission **[inferred]**.

## 2. Microphone

### Path **[verified]**

The guest records from the host device in `audiosettings.plist`'s `sim_input_device_uid` (default:
the host default input). A command-line tool needs an `AVAudioSession` category of `PlayAndRecord`.
Without it, `AudioQueueStart` fails with -66628 and coreaudiod logs "No kDeviceInput streams".

### Permissions **[verified from logs]**

When an in-device process records, host `coreaudiod` asks tccd for `kTCCServiceMicrophone` and
`kTCCServiceAudioCapture` on behalf of the guest PID. The responsible process is
**`com.apple.CoreSimulator.SimulatorTrampoline`**
(`CoreSimulator.framework/Versions/A/XPCServices/SimulatorTrampoline.xpc`), not Simulator.app,
simee or simctl. On this Mac the grant already existed, and real mic noise came through (about
-58 dBFS).

On a fresh CI image, the first recording needs SimulatorTrampoline to hold the Microphone grant.
MDM PPPC profiles can deny but not allow Microphone, so the grant comes from a one-time GUI click
when the image is built, or from editing TCC.db, which needs SIP off **[inferred]**.

Inside the device, its own TCC also applies. A bare CLI got "Unable to construct an identity to
kTCCServiceMicrophone" but still recorded. Apps use `simee privacy grant microphone <bundle>`.

### Feeding the microphone programmatically

- **Driverless: not possible through routing [verified].** I published a non-private aggregate whose
  only input is a muted tap of a host player process. Other processes could see it, it carried the
  player's sine, and nothing reached the speakers. Then:
  - The router refused to list or route to it (CanBeDefault 0).
  - Pinning it through `audiosettings.plist` made the guest build its per-process aggregate
    (`Built valid aggregate 158`), but `AudioQueueStart` failed with -66628.

  The guest's design wraps the chosen device in its own aggregate, and a tap-backed aggregate inside
  another aggregate did not run **[inferred cause]**.
- **Side effect worth knowing [verified]:** a public aggregate created through
  `AudioHardwareCreateAggregateDevice` outlived its creator when the creator was killed. I removed
  the stale device with `AudioHardwareDestroyAggregateDevice`. Code that publishes one must destroy
  it on every exit path.
- **With a loopback driver:** per-device input works by pinning `audiosettings.plist` to the
  device's UID **[verified with BlackHole]**, even though the router refuses BlackHole. Isolating
  two simulators still needs one loopback device per simulator, because a pin selects a device, not
  channels.
- **Driverless, per app [inferred, not built]:** inject a dylib, the same way as the camera poster,
  that replaces the input side of `AURemoteIO` / `AVAudioEngine.inputNode` / `AudioQueueNewInput`
  with PCM read from a file or socket. This needs no host change, but each audio API has to be
  hooked and system apps cannot be covered.

## 3. Camera

- **No native camera [verified].** In-device `camprobe` saw 0 video and 0 muxed devices, and
  `defaultDeviceWithMediaType:video` returned nil. The audio list did show one microphone entry.
  This matches auto-mobile's `report-stock-ios26.5.json`.
- The runtime's CMCapture has `FigCaptureSourceSimulator`, `FigCaptureSessionSimulator` and
  `FigFlashlightSimulator`, whose sink functions are stubs (for example
  `captureSessionSimulator_FileSinkStartRecording` is a single return). SimulatorKit,
  CoreSimulator, its simdeviceio bundles and Simulator.app contain no camera or AVCapture strings,
  apart from Simulator.app's unrelated `resetViewportCameraMenuItem`. No private API provides
  frames.
- **Dylib injection through simee works [verified].** I built auto-mobile's
  `ios/CameraPosterInjection` into scratch, installed `CameraFixture.app` on simee-test and granted
  camera. I then ran `simee call launch` with
  `env: {DYLD_INSERT_LIBRARIES: <libAMCameraPoster.dylib>, AM_CAMERA_POSTER_QR: "simee-probe-42"}`.
  simee passes `environment` straight into `launchApplicationWithID:options:`, so no `SIMCTL_CHILD_`
  prefix is needed. The fixture reported:
  - discovery count 1, default device present, device input created, preview content true;
  - metadata QR strings `simee-probe-42`;
  - a 1080×1440 photo.

  Afterwards I uninstalled the app and revoked the grant.
- auto-mobile today merges `SIMCTL_CHILD_DYLD_INSERT_LIBRARIES` plus `SIMCTL_CHILD_AM_CAMERA_POSTER_*`
  into every app launch that goes through its launch path, when a poster is registered for the
  simulator (`src/features/utility/ios/iosCameraPosterLaunch.ts`). It skips `com.apple.*`.
- **Device-wide injection via `launchctl setenv DYLD_INSERT_LIBRARIES` inside the device (observed by the
  network-shaping test):** processes launchd starts afterwards inherit it, including apps launched from the
  home screen or by XCUITest, so a camera poster can be set once per device. Already-running processes
  need a restart, and ExtensionKit extensions (WebKit's processes) get a sanitized environment and are
  not covered.

## 4. Speaker routing per device without a driver

**[verified]** options for sending one device to a null sink:
1. A muted process tap on the device's PIDs. Nothing reaches any host device, and the tap still
   delivers the samples, so it doubles as capture.
2. `sim_volume = 0` in `audiosettings.plist`. Output is silent, read when a client starts.
3. A pin to a device nobody listens to, via the plist. This still needs a spare device.

None of these changes the host default devices or affects other simulators.

## Recommended simee API

All methods go in `crates/sim-api` (socket, CLI and desktop at once). Routing and settings code goes
in `sim-coresim` (the selector boundary), and tap and Core Audio code in a new FFI module with
`// SAFETY:` comments. Check every private call with `respondsToSelector:` first.

| Method | Params | Mechanism |
| --- | --- | --- |
| `audio_state` | device | Reads the HostRoute port (`guest*HostDeviceUID`, `effectiveDefault*`, `availableHostDevices` with scope and channels) and `audiosettings.plist` (UIDs, `sim_volume`). Also returns all host devices from Core Audio, so callers can see devices the router hides. |
| `audio_route` | device, scope(input\|output), uid \| "default", via?(router\|settings) | `router` (default): `routeGuestDeviceScope:toHostDeviceUID:` with `"default"` → `__sim__hostUseSystemDefaultDeviceUID`, then **read back** and fail with `audio_route_rejected` if the UID differs. `settings`: write `audiosettings.plist` (accepts scope-3 devices like BlackHole) and return a note that only clients started afterwards follow it. simee keeps a per-device desired route and re-applies it after boot and on `availableDevicesChangedCallback`. |
| `audio_volume` | device, level(0–100) | Writes `sim_volume`; 100 is unity gain and 0 silences. Read at client start. |
| `audio_capture_start` | device, path?, sample_rate?, mute?(bool), include_pids? | A process tap of every in-device Core Audio client, kept current by watching `kAudioHardwarePropertyProcessObjectList` and rewriting `kAudioTapPropertyDescription`. Writes float32 or 16-bit PCM (WAV) to `path`. Returns `capture_id`. |
| `audio_capture_stop` | capture_id | Stops the tap, destroys the tap and private aggregate, and returns `{frames, duration_ms, rms, peak, path}`. |
| `audio_mute` | device, on | A muted tap that discards samples (null sink). |
| `audio_inject` | device, path \| pcm_base64, sample_rate?, device_uid | Only with a host loopback device: pin the device's input to `device_uid` (settings mode), play PCM into it, then restore. Otherwise fail with `audio_injection_needs_loopback_device`. |
| `launch` (existing) | add `inject?: [dylib]` | Merges into `DYLD_INSERT_LIBRARIES`. Plus a per-device registry (`camera_poster {device, image_path \| qr}`) so every simee launch carries the poster env. Device-wide via `launchctl setenv` once the main session's result confirms it. |

Notes:
- Long-running captures live in the simee server process, so its responsible app needs the System
  Audio Recording permission. Report `audio_capture_permission` when the tap returns silence or
  errors. Recording any input device, the loopback path, needs Microphone for the guest's
  responsible process, SimulatorTrampoline.
- `audio_capture_*` streams through a file path rather than JSON lines; the reply carries only
  metadata and levels.

## How auto-mobile would use it

- **Capture (`captureAudio`, #13629):** call simee `audio_capture_start/stop` over the unix socket
  (`$SIMEE_SOCKET`) instead of `audio-loopback-helper capture`. This removes the BlackHole
  requirement and the "capture hears every simulator" limit, and makes concurrent per-simulator
  capture possible (#13632/#13633). Without simee, the same tap code could ship as an
  `audio-loopback-helper tap --pid …` mode, since it is all public macOS 14.2+ API.
- **Inject (`injectAudioInput`, #13628):** keep `audio-loopback-helper play`, but give it an explicit
  output UID. Pin per device with simee `audio_route via=settings`, which works for BlackHole where
  the router does not, and serialize per loopback device. Driverless injection would need an in-app
  audio-input hook dylib (new work, the same shape as CameraPosterInjection).
- **Camera:** keep CameraPosterInjection. For simee-booted devices, launch through simee
  `launch {env}` (verified), or the future `camera_poster` registry, rather than simctl with
  `SIMCTL_CHILD_`. Use device-wide `launchctl setenv` if the main session confirms it works.
- **Null sink in CI:** `audio_mute` per device, or `audio_volume 0`.

## Still impossible without a driver or root

- Supplying microphone audio to an unmodified app through a host device. The guest only opens host
  devices that its aggregate can wrap, and the only driverless virtual input (a tap aggregate) fails
  there.
- Exposing a camera device to unmodified, non-injected apps. That includes Apple system apps, which
  refuse injection, and possibly XCUITest-launched apps unless device-wide `launchctl` injection
  works.
- Granting Microphone (SimulatorTrampoline) or System Audio Recording (the tapping app) on a fresh
  host without a GUI click or a TCC.db edit; MDM cannot allow these **[inferred]**.
- Restarting `coreaudiod` (needs sudo). The behavior of pins and taps across a `coreaudiod` restart
  is unmeasured.

## Unresolved

- Whether running clients follow a live `audiosettings.plist` change, or only new ones.
- Two concurrent devices pinned to two different loopback devices: no second device on this host,
  and I was limited to one test device.
- `launchctl setenv DYLD_INSERT_LIBRARIES`: works for launchd-started processes and relaunched apps; ExtensionKit extensions are not covered (observed, see the table above).
- iOS 27.x runtimes: everything above was measured on iOS 26.5 only. auto-mobile's #13631 used
  `SimDeviceAudioClient` on iOS 27.0, with matching routing behavior.
