# Development

## Requirements

- macOS on Apple silicon to run devices: Xcode (`xcode-select -p`, or `DEVELOPER_DIR`) with at
  least one simulator runtime for iOS; for Android, an SDK system image (`emu -provision` fetches
  one, no Java needed).
  Xcode 26 and 27 both work, and several can be installed side by side: `DEVELOPER_DIR` picks one
  per run. CoreSimulator is the system's (`/Library/Developer/PrivateFrameworks`) whichever Xcode
  is selected; SimulatorKit is the selected Xcode's, in `Contents/SharedFrameworks` from Xcode 27
  and `Contents/Developer/Library/PrivateFrameworks` before it (tried in that order). Xcode 27.2's
  SimulatorKit keeps every HID builder, the HID client class and selectors simee uses, and builds
  byte-identical messages to Xcode 26.6's apart from timestamps.
- Rust 1.99.1 (pinned in `rust-toolchain.toml`).
- On Linux the shared core, the API, the CLI and the desktop window build and test (CI runs them
  there); there are no devices to run, since both runtimes need macOS.


Build and check commands, the working rules and the code map are in [AGENTS.md](../AGENTS.md).

## Build times

Dependencies build unoptimized in the `dev` profile except the few on a runtime path (the kernel's
gunzip, the NAT, checksums, base64, PNG, audio), which are listed in the root `Cargo.toml`: a
dependency that a hot loop in `emu` or `simee` calls into belongs on that list, or a debug `emu`
gets slow. The desktop's `cargo test` builds Rune unoptimized too; its `cargo build`/`run` keep
opt-level 2. Numbers and what was tried are in [benchmarks.md](benchmarks.md#build-times-local).

## Harness

[auto-mobile](https://github.com/kaeawc/auto-mobile) maps both the public and private Apple
simulator APIs and is the reference for behavior. Three layers check simee against it and against
`simctl`:

| Check | What it proves |
| --- | --- |
| `cargo t` | pure logic: selection, gestures, key maps, pixel conversion |
| `SIMEE_E2E_UDID=<udid> cargo nextest run -p simee --test e2e` | simee commands on a real device; identity matches `simctl` |
| `scripts/parity.sh <udid>` | identity, screenshot size and pixels, and app list match `simctl` |
| `scripts/automobile-harness.sh <udid>` | auto-mobile acquires, observes and drives a simee-booted device (private daemon; touches no other daemon) |

Android checks (Apple silicon): `cargo run -p emu -- -avd <name> -show-kernel -verbose`, then
`adb -s emulator-5554 shell getprop sys.boot_completed`;
`SIMEE_E2E_AVD=<avd> cargo nextest run -p simee --test e2e_android`;
`scripts/automobile-harness-android.sh emulator-5554` (AutoMobile's tools against emu).

Many devices of both platforms at once: `scripts/automobile-farm.sh <udid> emulator-5554 emulator-5556`
runs each harness through one private AutoMobile daemon, one session per device.

## Linters

Scripts, docs and workflows have their own linters, each configured by a file at the root.
`scripts/lint.sh` runs them: CI's `docs-scripts` job does, and so does `scripts/check.sh`, first,
before its docs-only exit. A linter that isn't installed prints a skip line (`--require` makes it
fail, as in CI). Name tools to run just those: `scripts/lint.sh shfmt shellcheck`.

| Tool | Checks | Config |
| --- | --- | --- |
| [shellcheck](https://www.shellcheck.net) | every shell script in the tree (`*.sh`, `.githooks/*`, sh and bash shebangs), with a few optional checks on | `.shellcheckrc` |
| [shfmt](https://github.com/mvdan/sh) | the same scripts' formatting: 2-space indents, `case` branches indented (`shfmt -w <file>` fixes it) | `.editorconfig` |
| [lychee](https://lychee.cli.rs) | every Markdown link to a file in the tree, and the heading it points at; offline, so web links aren't fetched | `lychee.toml` |
| [typos](https://github.com/crate-ci/typos) | misspellings in code, docs and file names (`typos -w` fixes them) | `_typos.toml` |
| [actionlint](https://github.com/rhysd/actionlint) | the workflows, with shellcheck over their `run:` blocks | none |
| [yamllint](https://yamllint.readthedocs.io) | every YAML file | `.yamllint.yml` |

On macOS: `brew install shellcheck shfmt lychee typos-cli actionlint yamllint`. CI installs pinned
versions in `.github/workflows/ci.yml`'s `docs-scripts` job; bump them there.

Fix what a linter reports rather than silencing it. A word spelled that way on purpose goes in
`_typos.toml` with the reason; a shellcheck finding that is wrong for one line gets a
`# shellcheck disable=SCxxxx` directive on that line with a comment saying why. A file excluded
in a config while another branch owns it carries a TODO naming that branch: drop the exclusion
when it merges.

