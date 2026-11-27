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

Android checks (Apple silicon): `cargo run -p simee-emu -- -avd <name> -show-kernel -verbose`, then
`adb -s emulator-5554 shell getprop sys.boot_completed`;
`SIMEE_E2E_AVD=<avd> cargo nextest run -p simee --test e2e_android`;
`scripts/automobile-harness-android.sh emulator-5554` (AutoMobile's tools against emu).

Many devices of both platforms at once: `scripts/automobile-farm.sh <udid> emulator-5554 emulator-5556`
runs each harness through one private AutoMobile daemon, one session per device.

## Install

From crates.io (`--locked` builds with the dependency versions simee was tested with):

```bash
cargo install --locked simee       # the CLI, `simee serve` (the API) and `simee mcp`
cargo install --locked simee-emu   # `emu`, the Android emulator; simee finds it beside itself
# macOS: Hypervisor.framework only runs binaries with its entitlement (this repo's file)
codesign -s - -f --entitlements scripts/hypervisor.entitlements ~/.cargo/bin/emu
```

Or a binary archive from the [GitHub releases](https://github.com/kaeawc/simee/releases):
`simee-vX.Y.Z-<target>.tar.gz` holds `simee` and `emu` for `aarch64-apple-darwin` (emu already
signed), `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.

## Releasing

Every workspace crate is published to crates.io under a `simee-` name (`simee-core`,
`simee-emu-vmm`, …; the CLI is `simee`, the emulator `simee-emu`), at the one workspace version,
with `=X.Y.Z` pins between them. Each library is named after its package, so a downstream user
writes `simee-core = "0.1"` and `use simee_core::...`. Workspace code keeps the old names:
`[workspace.dependencies]` maps the key `sim-core` to the `simee-core` package, so a dependent
says `use sim_core::...`, while a crate's own tests and examples use its package name
(`simee_core::`) and cargo's `-p` takes the package name (`cargo test -p simee-core`). Every
crate ships the MIT text through a `LICENSE` symlink to the root one. The desktop app
(`desktop/`) is not published.

1. Add what changed under `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md) as you go.
2. `scripts/release.sh 0.2.0` branches from `origin/main`, bumps the workspace version and the
   pins (and both lockfiles), moves the Unreleased entries under `## [0.2.0] - <date>`, pushes
   `release/v0.2.0` and opens the PR.
3. Merging it runs [release.yml](../.github/workflows/release.yml): binaries for
   `aarch64-apple-darwin` (`emu` ad-hoc signed with `scripts/hypervisor.entitlements`) and Linux
   x86_64 and aarch64, `cargo package --workspace` on macOS (each crate built from its `.crate`),
   `scripts/publish.sh` with a crates.io token from trusted publishing, then the `v0.2.0` tag and
   GitHub release with the archives and the CHANGELOG section as notes.

Every step can be rerun: `scripts/publish.sh` skips crates whose version is already on crates.io,
and an existing release gets its assets replaced. Dry runs: `scripts/publish.sh --dry-run`
(packages and verifies what is missing, uploads nothing), or run the Release workflow by hand
(builds and packages on any branch, publishes nothing).

Trusted publishing (no long-lived token in the repository): the `publish` job runs in the
`crates-io` GitHub environment with `id-token: write`, and `rust-lang/crates-io-auth-action`
trades the job's OIDC token for a short-lived crates.io token. Each crate's settings on crates.io
name the trusted publisher: repository `kaeawc/simee`, workflow `release.yml`, environment
`crates-io`. crates.io only offers that once a crate exists, so a new crate's first version is
published with a personal token and gets its trusted publisher added afterwards.

First publish (no simee crate on crates.io yet; the Release workflow skips with a notice until
one is):

1. On crates.io, create an API token scoped to `publish-new` and `publish-update`, with the
   crate name pattern `simee*` and a short expiry. `cargo login` with it.
2. From an up-to-date `main` checkout: `scripts/publish.sh --dry-run`, then `scripts/publish.sh`.
   crates.io rate-limits new crates, so expect HTTP 429 partway through; wait and rerun, it
   skips what is already up.
3. For every crate, add the trusted publisher above in its crates.io settings.
4. Delete the token on crates.io and `cargo logout`.
5. Push the `vX.Y.Z` tag at the commit you published from (`git tag v0.1.0 <sha> && git push
   origin v0.1.0`): the Release workflow builds the binaries and creates the GitHub release, and
   its `scripts/publish.sh` skips every crate, now that each one is up.

