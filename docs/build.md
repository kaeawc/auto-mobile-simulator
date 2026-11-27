# Local build time (#206)

Measured 2026-11-27 on an M-series Mac (16 CPUs, rustc 1.99.1, macOS 26, ld-prime `ld-1267`)
shared with other builds: host load ran 110-120 (as `spectra load` reports it) for every row, so
absolute times are pessimistic; the comparisons are interleaved on the same host. Every row is a
separate `CARGO_TARGET_DIR`, `cargo build --workspace` (dev profile), one-line change made by
appending a comment to `apps/simee/src/main.rs` (leaf) or `crates/sim-core/src/lib.rs`, medians
of 3 (spread in brackets). Incremental rows follow an uncounted warm-up.

| Measurement | Before | After | Change |
|---|---|---|---|
| Clean `cargo build --workspace` | 10.4 s [10.2-12.4] | 10.7 s [10.6-11.4] | none (within spread) |
| Incremental, leaf change (`simee`) | 0.6 s [0.6] | 0.6 s [0.6] | none |
| Incremental, `sim-core` change | 2.2 s [1.8-2.5] | 1.3 s [1.3-1.4] | -40% |
| `cargo test --workspace --no-run`, clean | 12.3 s [11.8-12.3] | 12.0 s | none |
| `cargo test --workspace --no-run`, `sim-core` change | 2.9 s [2.6-3.1] | 1.9 s [1.9] | -35% |
| `scripts/check.sh` (warm; fmt, 2x clippy, tests) | 28.5 s [28.5-28.5] | 30.1 s [28.8-31.4] | none |
| `desktop/` clean `cargo build` (own workspace, not changed) | 86 s, 103 s (2 runs) | n/a | n/a |
| `desktop/` incremental, `main.rs` change | 0.9 s [0.8-0.9] | n/a | n/a |

"Before" is `main` at ad19556, which already carries a tuned dev profile: `debug =
"line-tables-only"`, dependencies at `debug = false`, `opt-level = 0` for build scripts and proc
macros, and hand-picked optimized hot-path crates. `scripts/check.sh` is dominated by running the
tests (27-35 s of 28-48 s; cold, the two clippy passes add 11 s), not by compiling.

## What won

`[profile.dev] codegen-units = 16` (dev default is 256, with incremental on). Rebuilding after a
`sim-core` change recompiles every crate that depends on it, and with 256 units each the
incremental session has far more units to hash, reuse and link (I did not profile which part
dominates). Sweep on the `sim-core` change, then `test --no-run`: 4 units 1.2 s / 1.6 s,
16 units 1.3 s / 1.6 s, 64 units 1.5 s / 1.9 s, 256 (default) 2.2 s / 2.9 s. 16 was kept: 4 slows
the clean build (11.3 s), 16 and 64 do not. It changes no generated behaviour at `opt-level = 0`;
the few `opt-level = 1-3` crates get slightly better inlining. `profile.test` and `profile.ci`
inherit it.

## What did not help (all against the same baseline, medians of 3)

- Linker: the default is ld-prime (`cc -Wl,-v` reports `ld-1267`). Rust's bundled `ld64.lld`
  (`-C link-arg=-fuse-ld=<sysroot>/lib/rustlib/aarch64-apple-darwin/bin/gcc-ld/ld64.lld`) gave
  clean 10.2 s, leaf 0.7 s, `sim-core` 1.9 s, test 2.8 s: no win, and it would be one more thing
  to install on CI. No `.cargo/config.toml` change. (The only other `lld` on the host is the
  Android SDK's, a generic multi-flavor binary; not tried.)
- `split-debuginfo = "unpacked"`: clean 10.4 s, leaf 0.6 s, `sim-core` 2.0 s, test 2.9 s. The
  dev profile already emits only line tables, so there is almost no debuginfo to split.
- `debug = 0` for dev (drops line tables too): clean 9.6 s (-7%, within the host's spread) and
  `sim-core` 1.9-2.0 s. Not kept: it loses file and line numbers in backtraces for no measured gain.
- Dependency opt-levels: the existing overrides are documented in `Cargo.toml` (about 40% of a
  clean build's CPU at opt-level 2 for everything); I did not re-tune them.
- Dependency feature trimming: workspace dependencies are already `default-features = false`
  with explicit feature lists, and the workspace builds in 124 units, so I found nothing worth
  trimming and did not try it. No Cargo.lock change.
- sccache: not installed (not installed for this change either). With a 10 s clean build there is
  little for a shared compiler cache to save across worktrees; `desktop/` (86-103 s, 300+ units,
  Rune's GPU stack) is where it would matter.

## Which crates dominate (`cargo build --timings`, clean, 124 units, 55 s of CPU in a 10 s build)

Longest units: `zlib-rs` 3.9 s (opt-level 3, on the critical path at the start), `emu-vmm` 3.6 s
(opt-level 1), `serde_core` 1.9, `syn` 1.9, `sim-api` 1.8, `miniz_oxide` 1.7, `cc` 1.5,
`clap_builder` 1.5, `sim-core` 1.5. The wall time is a chain: `zlib-rs` or `serde_core`, then
`sim-core`, `sim-api`, `emu-vmm`, linking; no one crate is worth splitting.

`desktop/` (clean 86 s, 305 units): `rune` 38 s (starts at 45 s, so it is the tail), `syn`
14 s, `naga` 13 s, `read-fonts` 11 s, `rune-text` 10 s, `moxcms` 9 s, `serde_derive` 9 s.
