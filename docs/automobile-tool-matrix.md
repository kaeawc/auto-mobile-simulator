# auto-mobile tool matrix on a simee-booted device

Date: 2026-11-22. Device: `simee-harness` (D678D16A-EEDB-4AC6-BD22-FEE35C863FC8), iPhone 17, iOS 26.5,
booted headless by simee. auto-mobile 0.0.95+g9064fa8e989a run from source (`bun src/index.ts --cli`)
through one private daemon (its own socket, PID, lock, token, data, log and DB paths, same setup as
`scripts/automobile-harness.sh`), with Xcode 26.6. The host load average ran from 470 to 730 during the sweep.

Target app: auto-mobile's `ios/Playground`, built Debug for the simulator into a scratch DerivedData
folder, installed with `simee install`, launched with `simee launch` and `launchApp`. The build used
`xcodebuild` directly, with the same flags as `scripts/ios/xcode-build.sh` (generic iOS Simulator
destination, `CODE_SIGNING_ALLOWED=NO`, `ARCHS=arm64`, `ONLY_ACTIVE_ARCH=YES`,
`COMPILER_INDEX_STORE_ENABLE=NO`). The script itself was not run because it regenerates every tracked
`ios/*/project.pbxproj` with xcodegen.

Comparison: a temporary `simee-compare` device (iPhone 17, iOS 26.5) booted with `xcrun simctl boot`,
with Playground installed by `simctl install` and acquired in the same private daemon. Every failure
that could plausibly depend on how the device was booted was re-run there. The device was deleted afterwards.

## Summary

109 tools listed by `--cli help`. 85 were run against the device and 24 skipped.

| Result | Count |
| --- | --- |
| pass | 67 |
| partial (some actions pass, the rest fail for reasons unrelated to simee) | 1 |
| refused on iOS by design (platform or form factor) | 6 |
| fail (host setup, the target app, CLI limits or auto-mobile; none caused by simee) | 11 |
| skipped (Android-only, or would create or delete devices) | 24 |

**No failure was caused by the device being booted or driven by simee.** Every failure that was
re-run on the simctl-booted device failed the same way, with the same error text.

## Matrix

"Same on simctl" means the tool was re-run on `simee-compare` with the same result.

### Observation and inspection

| Tool | Params | Result | Notes / cause |
| --- | --- | --- | --- |
| getApple | `--udid <harness>` | pass | `automationReady` (bootCompleted, runnerReady, sessionBound) in 36 s on first acquire, 34 s after a fresh `simee boot`. The simctl device took 13 s (host load varies; not attributed). |
| observe | default; `--waitFor {"for":"appear","text":"Biometrics"}`; `--layer prototype` | pass | Screen 402x874 pt, SDK safe-area insets (top 62, bottom 34), `screenIdentity` from the SDK. |
| hitTest | `--x 200 --y 300` | pass | First candidate: `Preferences` UITableViewCell. |
| listApps | default | pass | 38 apps, Playground included. |
| listDevices | (no session) | pass | |
| listDeviceImages | `--platform ios` | pass | Result was 91 KB and went to an artifact file. Without `--platform`, the CLI rejects the call (required param). |
| getDeviceState | default | pass | DND and connectivity reported unsupported on iOS, as documented. |
| getIosSimulatorCapabilities | iPhone-17 / iOS-26-5 ids | pass | |
| inspectPackageSigning | `--appId Playground` | pass | adHoc, cdHash reported. |
| getDeepLinks | Playground; `com.apple.mobilesafari` | pass | Playground has no URL schemes. Safari lists http/https/etc. |
| getAppPermissions | Playground; `["photos"]` | pass | |
| getNotificationPolicy | Playground | pass | Warns that the app never requested authorization. |
| getLogs | `--maxLines 20`; `--buffers ["crash"] --since {"seconds":1800}` | pass | `--since sessionStart` timed out twice when the session was about 50 min old: `Unified log collection timed out after 20000ms`. On the simctl device, with a 63 s old session, it passed. Cause: log window size plus host load, not simee. |
| compareScreenshot | `record`, then `compare` baseline `simrs-home` | pass | 1206x2622, diffRatio 0. |
| snapshotOf | `--rectangle {16,100,386,300}` | pass | 1110x600 PNG. |
| accessibility | read; `--voiceover true`; `--voiceover false` | pass | VoiceOver turned on and then off; read back as `voiceover` / `unknown`. |
| accessibilityAudit | `--appId Playground --maxIssues 5` | pass | engine xcuitest, 2 issues. `--failOn` is rejected for this engine (usage error). |
| doctor | `--ios true` | pass | The harness CtrlProxy runner is `compatible`. Warnings concern only the developer account and host fleet pressure. |
| getSessionRequirementCapability | ios requirement | fail | `This daemon is not running the session broker; start it in session-broker mode.` Daemon configuration, not simee. |

### Gestures and input

| Tool | Params | Result | Notes / cause |
| --- | --- | --- | --- |
| tapOn | `--selector {"text":"Demos"}`, "Forms & Input", "Biometrics", "Settings" | pass | A flat `--text` is rejected; it must go under `selector`. |
| tapAt | `--x 50 --y 50 --coordinateSpace percent` | pass | |
| tapAny | `--action tap` | pass (retry) | First call: `request_tap_coordinates exceeded execution bound 4495ms in phase xcuitestGesture`. Passed on retry. Host load. |
| swipeOn | `--direction up`; `--lookFor {"text":"Forms & Input"}` | pass | |
| swipeAt | `{200,700}` to `{200,300}` | pass | |
| dragPath | 3-point polyline | pass | |
| dragAndDrop | "SDK Status" to "Biometrics" | pass | The gesture was dispatched. Playground has no drop target, so only dispatch was checked, not the drop itself. |
| pinchOn | `--direction out` | pass | |
| multiTouch | 2-finger tap at 50%/50% | pass | |
| shake | default | pass | |
| rotate | landscape, then portrait | pass | "Successfully rotated from portrait to landscape" and back. |
| sendKeys | type `Héllo simee 123` into Name; clear + type + tab | pass | Unicode typed and read back (`verified: true`). |
| selectAllText | focused Name field | pass | |
| keyboard | `detect`, `close`, `listProfiles` | pass | detect and close pass. listProfiles is Android-only, by design. |
| clipboard | `copy`, `get`, `clear` | pass | `get` returned "simrs clip". |
| pressButton | home, back, volume_up, volume_down, power, action, camera | pass (partial) | home/back/volume/power pass. home failed once right after recentApps (foreground verification looked for the App Switcher) and passed on retry. **action**: `Button press outcome is indeterminate ... (Press button timed out after 5000ms)`. **camera**: `camera button requires a device with Camera Control`. Both are the same on simctl, so this is auto-mobile/runner behavior, not simee. |
| wakeAndUnlock | default | pass | "no swipe was needed". |
| homeScreen | default | pass | |
| recentApps | default | pass | Passed twice on the simee device. **Failed twice on the simctl device** (`iOS App Switcher was not detected after the gesture`). An auto-mobile flake, not a simee gap. |
| highlight | `--text Discover` | fail | `Expected one visible Simulator or Device Hub window named simee-harness, found 0.` Same on simctl (`... named simee-compare, found 0`). Needs a visible Simulator.app window. See gaps. |
| systemAlerts | `get`, `setPolicy allow`, `clear` | pass | |
| siri | `--phrase "What time is it"` | pass | |

### App lifecycle and install

| Tool | Params | Result | Notes / cause |
| --- | --- | --- | --- |
| openLink | `--url https://example.com` | pass | Safari came to the foreground on the URL. |
| launchApp | Playground; `--coldBoot true`; `--prototype true` | pass | |
| terminateApp | Playground | pass | |
| appLifecycle | `background`, then `killBackgrounded` | pass | |
| crashApp | Playground | pass | "via ios_simulator_sigabrt (OS crash confirmed)". |
| uninstallApp | Playground | pass | `simee apps` confirmed it was gone. |
| installApp | Playground.app from DerivedData | pass | `simee apps` confirmed it was back. |
| putAppFile | app_containers/documents `simrs/a.log` | pass | |
| resetAppLogs | documents `["simrs/a.log"]` | pass | |
| executePlan | `--platform ios`, 3 steps (launchApp, tapOn Settings, tapOn Discover) | pass | 3/3 steps. `platform` is required on the CLI. |
| recordToolCalls | `begin` | fail | `A one-shot CLI call cannot hold a recording because its MCP connection closes after the call.` CLI limit. |
| exportPlan | `--planName simrs` | fail | `No active recording on this MCP connection.` Follows from the recordToolCalls failure; CLI limit. |
| recordDeviceGestures | default | refused | `only supported on Android right now (got ios)`. |

### Device settings and state

| Tool | Params | Result | Notes / cause |
| --- | --- | --- | --- |
| setDeviceState | location static 37.3349,-122.009; biometrics enrolled; cameraPoster qr / clear; networkCondition offline per app / reset; clock advance | pass | All pass except clock: `Real clock control/read-back is unsupported on iOS simulators`, by design. The network-extension filter acknowledged the per-app offline rule (network filter works). |
| setDeviceResources | liveActivities disabled, then enabled | pass | Slow: about 135 s each (host load). |
| reconcileDeviceResources | `{"widgets":"enabled"}` | pass | No drift, `verification: current_boot`. |
| displayConfig | read; theme dark/light; contentSize extra-large/large | pass | Reverted afterwards. |
| changeLocalization | fr_FR + Europe/Paris, then en_US + America/Los_Angeles | pass | `--appId` is Android-only, by design. |
| statusBar | override 9:41 / battery 100; get; clear | pass | |
| setAppPermissions | grant `["photos"]` | pass | "not verified: effective full read/write Photos access could not be verified" (TCC limit). `notificationsEnabled` is unsupported on iOS. |
| getPreference | userDefaults `simrsTest` | pass | The first call failed while Safari was in front: `requires ... to be the foreground app` (by design). It passed with Playground in front. |
| setPreference | userDefaults `simrsTest` string | pass | Container-plist route (app not in front) passes and reads back. With Playground in front, the SDK route fails: `iOS UserDefaults SDK write failed: in a DEBUG build, configure StorageInspectionConfiguration(allowMutations: true)...`. That is the app's configuration. |
| resetKeychain | `--confirm true` | pass | Device-wide reset. Without confirm it refuses, by design. |
| deviceSnapshot | capture `simrs1` (app data + settings), restore, delete | pass | Restore brought `simrsTest` back to its captured value. |
| setNotificationPolicy | `--policyAccess true` | refused | `iOS does not expose app notification policy access`. |
| setPosture | `--posture closed` | refused | "This iOS simulator is not a foldable device." |
| setActiveDevice | `--deviceId <harness> --platform ios` | pass | |
| setToolEnabled | `--toolName highlight --enabled true` | pass | |
| killDevice | `{"name":"simee-harness",...}` | pass | Shut the device down. `simee boot` brought it back in 10 s. getApple, launchApp, tapOn, observe and `simee screenshot` all worked afterwards. |

### Notifications, biometrics, telephony, audio, performance, recording, prototypes

| Tool | Params | Result | Notes / cause |
| --- | --- | --- | --- |
| biometricAuth | `match --modality face`; `match`, then the app's "Consume Override" | pass | End to end: the Playground Biometrics demo showed "Consumed: success". |
| postNotification | title/body, `--appId Playground` | fail | `The app has not been granted notification authorization on this simulator`. Playground never calls `requestAuthorization`. Same on simctl. Target app. |
| systemTray | `open`, `close`, `find`, `list`, `clearAll` | partial | open and close pass. find found nothing, because nothing could be posted (see postNotification) and Notification Center over an app hides its content. list is Android-only. clearAll requires `notification.appId`. |
| phoneCall | `call 5551234` | fail | `phoneCall on iOS failed in the app's AutoMobile SDK: callkit_timeout.` Same on simctl. Target app / SDK. |
| sendSms | `5551234 "hi"` | fail | `sendSms on iOS failed in the app's AutoMobile SDK: notification_timeout.` Same on simctl. Target app (no notification authorization). |
| injectAudioInput | 440 Hz tone, 3 s | fail | `The host default input 'MacBook Pro Microphone' is not a loopback device`. Host setup. |
| stopAudioInput | (no injection id) | fail | Nothing to stop, because injectAudioInput failed. |
| captureAudio | `--seconds 2` | fail | `The host default output 'MacBook Pro Speakers' is not a loopback device`. Host setup. |
| performanceTrace | `start --template hitches` | fail | `xctrace with Xcode 26.6 never finishes saving a trace on an iOS 26.5 simulator ... Run AutoMobile with Xcode 27.x`. Host toolchain; auto-mobile gates it on the version. |
| performanceAudit | `--appId Playground` | refused | `On-demand audits run on Android only.` |
| videoRecording | start (maxDuration 30), drive with `simee swipe` and `simee tap`, stop | pass | MP4 1206x2622, 200 frames, 2.7 MB. simee input during recording was captured. |
| prototype | `show` text spec, `status`, `dismiss` | pass | Required `launchApp --prototype true`. `observe --layer prototype` saw "Hello simee". |
| exportPrototypeWeb | `--id simrs` | pass | 26 KB HTML. |
| focusNavigate | `--until {"text":"Biometrics"}` | refused | Android and tvOS only. |
| rotateCrown | `--delta 0.25` | refused | watchOS only. |

### Skipped (24)

| Tools | Why |
| --- | --- |
| hoverAt, sendBroadcast, backgroundWork, measureStartup, captureTrace, heapDump, mediaSession, windowMode, portReverse, killProfile, setSensor, seedContent, displayProfile, manufacturerProfile, setAppLinks, accessibilityAction, blePeripheral | Android-only, according to each tool's description |
| rotaryScroll, ambient, wearSurface, glassesInput | Wear OS / Android XR only |
| getAndroid | Android acquisition |
| provisionDevice | Creates devices; out of scope (would add to the shared device inventory) |
| deleteDevice | Permanently deletes a device; not run on the harness device, which other simee work expects to exist |

## simee cross-check

After the auto-mobile sweep, every simee command still worked on the device: `terminate`, `launch`
(printed a pid), `tap` (the Files tab opened), `screenshot`, `type`, `button home`, `appearance`,
`location`, `open-url`, `privacy grant camera`, `memory-warning`. `simee install` was the only install
path for the first Playground install, and `simee boot` re-booted the device after `killDevice`.
auto-mobile then acquired and drove the freshly simee-booted device without problems. Inputs sent with
simee during an auto-mobile `videoRecording` showed up in the recording.

## simee gaps found

No auto-mobile capability failed on the simee-booted device and passed on the simctl-booted one.
`getApple` returned the same capability inventory for both devices. After the daemon had seen
both, it also returned the same enabled and gated tool lists; the difference on first acquire came
from daemon state, and re-acquiring the harness gave the same lists as the compare device.

Differences worth tracking:

1. **No Simulator-window equivalent for `highlight`.** auto-mobile draws highlights over a visible
   Simulator.app (or Device Hub) window that has the device's name. A headless simee device has none:
   `Expected one visible Simulator or Device Hub window named simee-harness, found 0.` A headless
   simctl-booted device fails the same way, so this is not specific to simee. But if simee is meant to
   replace Simulator.app, then the simee desktop window, or a headless overlay hook, would need to be
   something auto-mobile can find. Not tested with the simee desktop window open.
2. **Acquisition time (unconfirmed).** `getApple` reached `automationReady` in 36 s and 34 s on the
   simee device and 13 s on the simctl device. The host load average was 470 to 730 and changed
   minute to minute, and the CtrlProxy runner state differed. One sample each, so this is not
   attributed to simee. Worth re-measuring on a quiet host.

Things that worked the same on both, or that simee did better:
- Observe insets: the simee device reported SDK safe-area insets (top 62, bottom 34). The simctl device, right after `simctl install` + `launchApp`, reported `insets.available: false` (heuristic, all zero) on two reads. This is probably SDK connection timing on the new device, not a simee effect.
- `recentApps` passed on simee and failed twice on simctl (auto-mobile flake).

## Reproducing

The private-daemon environment matches `scripts/automobile-harness.sh`; the sweep also set
`AUTOMOBILE_CLI_SESSION_IDLE_TIMEOUT_MS=3600000` so the session survived between calls. Raw per-call
outputs went to the session scratchpad and were not kept.
