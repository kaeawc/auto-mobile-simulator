# Network shaper overhead

`crates/sim-netshape/native/netshape.c` is interposed into every process of a boot-injected device
(about 120 per device, SpringBoard included) and sees every `read`, `write`, `send`, `recv` and
`connect` they make. Most of those calls are on fds the shaper leaves alone: files, pipes, unix
sockets, loopback. The goal: close to zero added cost when no rule applies, minimal cost when one
does, and the same shaping as before.

## Where the time went (main at 83c34cf)

| Call | Extra work per call |
| --- | --- |
| `send`/`sendmsg`/`sendmsg_x` on **any** socket, shaped or not | `getsockopt(SO_TYPE)`: the type check ran before the "is it shaped" check |
| `sendto`/`sendmsg` to a remote address | a full `match_rule`: reload check (mutex), learned-name scan (mutex, up to 256 entries), `snprintf` + `inet_pton` per CIDR rule (mutex) |
| shaped TCP send | up to three `getsockopt`, four or five mutex lock/unlock pairs (`reload` and `effective` each lock `g_lock`, the token bucket too), two or three clock reads |
| shaped receive | one or two `getsockopt`, two to four mutex pairs |
| profile reload | `fopen`/`fgets` (allocates); every thread that arrived during the reload waited on the lock |

`read`/`write` on unshaped fds were already cheap (one atomic load).

## How others do it

| Approach | Per-call cost | fd reuse, close, dup, fork | Signals and reentrancy | Complexity |
| --- | --- | --- | --- | --- |
| **trickle** (LD_PRELOAD bandwidth shaper; [trickle-overload.c](https://github.com/mariusae/trickle/blob/master/trickle-overload.c)) | linear `TAILQ` search by fd under a global mutex; `fcntl(F_GETFL)` per I/O update | type recorded at `socket()` (TCP only); `close` frees the entry; `dup`/`dup2` copy entries (buggy: `dup2` leaves the target's old entry); `fork` not handled | `initializing` flag, lock dropped around blocking libc calls | low |
| **libfaketime** ([source](https://github.com/wolfcw/libfaketime/blob/master/src/libfaketime.c)) | config re-read only when older than `FAKETIME_CACHE_DURATION` (default 10 s) | n/a (no fds) | `__thread` "don't fake" guard around real calls; init through a state enum and an error-checking mutex; known recursion bugs with jemalloc and `sem_open` | medium |
| **libfiu** ([fiu.c](https://github.com/albertito/libfiu/blob/master/libfiu/fiu.c)) | read-lock a global `pthread_rwlock` plus a lookup on every check once any point is enabled | n/a | `__thread rec_count` checked before any lock | low |
| **main's netshape** | see above | stale state after a close it can't see (`close$NOCANCEL`, `guarded_close_np`); `close` left the UDP timestamp flag set | `__thread g_depth`; mutexes on the hot path | low |
| **pf + dummynet / Network Link Conditioner** (kernel; `scripts/pf-shape.sh`) | per packet in the kernel, nothing per call in userspace | exact (shapes packets, not fds) | n/a | needs root; system-wide; no per-process or per-device scope |
| Per-fd table indexed by fd (bytes or words) | one load | as good as the lifecycle hooks that clear it; a generation counter per fd only helps if the close is seen | lock-free | low |
| Profile behind a mutex or rwlock | 20–100 ns uncontended, cache-line bouncing under contention | — | can deadlock if a signal handler re-enters on the same thread | low |
| Atomic pointer to an immutable snapshot | one acquire load | — | lock-free | reclaiming old snapshots needs RCU, hazard pointers or leaking |
| Userspace RCU / QSBR ([liburcu](https://liburcu.org/)) | free reads | — | QSBR needs every reader thread to register and announce quiescent states, which an injected library can't impose on threads it doesn't own | high |
| Seqlock ([Boehm](https://www.hpl.hp.com/techreports/2012/HPL-2012-68.html)) | two loads and a fence, retry on overlap | — | a reader spins while a preempted writer holds the sequence odd | medium |
| **Seqcount latch** ([kernel docs](https://docs.kernel.org/locking/seqlock.html)) | two loads and a fence on a stable copy | — | readers never wait: the writer updates one copy while readers use the other; safe if a reader interrupts the writer | medium; double storage |
| Pre-parsed rules | CIDR match is a `memcmp` plus a mask | — | — | low |

On Apple silicon, 32- and 64-bit C11 atomics are plain lock-free loads and stores.
`clock_gettime`/`mach_absolute_time` read the counter in userspace. Any system call (such as
`getsockopt`) is a kernel round trip costing several times more.

## Design

- **Per-fd state in one word** (`g_fd[fd]`, 32 bits): shaped, last-op-was-send, first-bytes-seen,
  timestamps-on, round-trip debt, socket type known, datagram, and the matched rule. One relaxed
  load answers "shaped? datagram? which rule?". Updates are compare-and-swap loops that write only
  when the value changes, so steady-state I/O doesn't write shared memory.
- **Socket type read once.** `getsockopt(SO_TYPE)` runs when a socket becomes shaped (at a remote
  `connect`/`connectx`, as before, and at the first remote `sendto`), never per I/O.
- **fd lifecycle hooks** keep the cache honest when a number is reused. `close`,
  `close$NOCANCEL` and `guarded_close_np` clear the state before the number can be reused.
  `socket`, `accept`, `dup` and a successful `dup2` clear the new fd. Because the type is re-read
  whenever an fd becomes shaped, a wrong cached type can only follow a stale "shaped" flag. main
  had that flag too, and these hooks narrow the ways it can go stale.
- **Profile published as a seqcount latch.** At reload, every rule's *effective* profile (base
  with the rule applied) is computed once and written to two copies. Readers pick the stable copy
  by the sequence's low bit, read relaxed atomics, and retry only if a publish overlapped. They
  never take a lock or wait for the writer.
- **Reload without blocking.** The 250 ms check is one clock read and one load. When it's due, one
  thread wins a compare-and-swap and re-reads the file (with `open`/`read`, no allocation, no
  `FILE`), while the others keep using the published profile. Only the process's first load is
  waited for, so the first connection still sees the profile. The periodic check uses `trylock`,
  so a child forked while another thread held the reload lock skips reloads instead of
  deadlocking.
- **Rules pre-parsed** at reload: `*`, address family, network bytes and prefix length. A learned
  name is looked up only when a hostname rule is reached, and not at all when there are no rules.
- **Destination cache.** For `sendto`/`sendmsg`/`sendmsg_x` to a remote address, the last
  (address → rule) per fd is cached in 256 slots. An entry is valid at one `g_match_gen`, which is
  bumped when rules are installed or a name is learned that changes a mapping, so a hit gives the
  same rule a fresh match would. Each entry has its own sequence number. A writer that finds it
  busy skips caching rather than wait, and a reader that sees a write in progress misses.
- **Token buckets** are one atomic "link free at" time per direction, advanced by
  compare-and-swap.
- **Host-testable.** The portable core compiles on Linux and macOS hosts with
  `NETSHAPE_HOST_TEST`. Only the Apple-only hooks are excluded: `connectx`, the batch calls, the
  DNS-SD, path and reachability hooks, and the extra close variants.

### What changes in behavior

The shaping is unchanged: the same rules, profile fields, file format, latency, bandwidth and loss
models. Only these edge cases differ:

- `close` now also clears the UDP "SO_TIMESTAMP enabled" and TCP debt flags. Before, a new UDP
  socket that reused a closed shaped one's number never turned timestamps on. Its datagrams were
  then delayed from the time they were read instead of when they arrived.
- New fds start unshaped even if their number was closed through a path the shaper didn't see.
  Before, such a number could carry a dead socket's "shaped" flag onto a file or pipe and pace it.
- When a periodic reload is due, threads other than the one reading the file keep the previous
  profile for the microseconds the read takes, instead of waiting for it.
- `pace` only clears the "last op was send" bit on a live fd. Before, a pace racing with `close`
  could mark the closed number shaped again.

### Remaining gaps

- Fds closed inside libsystem_kernel itself, duplicated with `fcntl(F_DUPFD)`, or received over
  `SCM_RIGHTS` aren't seen. Neither `main` nor this change hooks them, and the creation hooks
  above cover most reuse.
- fds ≥ 8192 are never shaped (unchanged).
- Matching (`connect`, a first or changed datagram destination, DNS lookups, a stream's first
  bytes) still takes mutexes, as before. Steady-state I/O doesn't.
- The reload check reads the clock on each shaped I/O, about 30 ns of `effective()`'s 50 ns on the
  Linux VM below. A coarser clock (`CLOCK_UPTIME_RAW`) could cut that on Apple silicon but hasn't
  been measured there.

## Numbers

Linux VM (4 vCPU x86-64, gcc 11.2, `-O2`). Three alternating runs of each build, as ranges. The
baseline is `main`'s `netshape.c` built on Linux with a shim (no interposing, `st_mtim`, no
`sa_len`, a fixed random) and driven by the same benchmark bodies. Neither build's hot paths
depend on the shim. On this VM `getsockopt(SO_TYPE)` measured 346–386 ns and `clock_gettime` 30 ns.

| Path (ns per call) | main | this change |
| --- | --- | --- |
| unshaped socket: the shaper's work around a `send` (gate + pace, no syscall) | 402–1073 | **14–17** |
| unshaped `send`+`recv` pair, minus the raw syscalls | 448–1509 | −49 to 117 (noise) |
| unshaped `write`+`read` pair, minus the raw syscalls | −83 to 166 (noise) | 13–64 (noise) |
| `effective(rule)` (includes the reload clock check) | 101–238 | 48–58 |
| shaped TCP: send gate + `pace` send + `pace` receive (no limits set) | 1798–4740 | **138–168** |
| UDP `sendto` to a remote address under 8 CIDR/name rules: the gate | 2386–6066 | **103–127** |
| `match_rule` uncached, 8 rules, 64 learned names | 1509–2052 | 309–377 |

Reproduce with:

```bash
cc -std=gnu11 -O2 -pthread -o /tmp/netshape_test crates/sim-netshape/native/netshape_test.c
/tmp/netshape_test bench
```

## Verification

- `cargo nextest run -p sim-netshape` builds `native/netshape_test.c` with the host C compiler and
  runs it on real host sockets. It covers:
  - parsing, including fgets-style line cutting and the rule limit;
  - reload, latch publication and the no-file case;
  - CIDR, IPv6, mapped-address and hostname matching;
  - fd state across connect, close, `socket`, `dup` and `dup2` (including a failed `dup2`);
  - the datagram gate (offline, loss) and when the destination cache hits and misses (rules, names,
    IPv6);
  - unshaped paths: no matches, no `getsockopt`, no state, no delay;
  - TCP pacing (bandwidth, round-trip owed, debt, loss stall) and the shared token bucket;
  - SNI/Host inspection;
  - the UDP delay line;
  - threads hammering the latch (no torn profile) and the destination cache (no wrong rule).

  A second test checks that the C shaper and Rust's `Profile::for_host` give 12 hosts the same
  effective profile.
- The C tests also pass under `-fsanitize=thread` and `-fsanitize=address,undefined`. Eleven
  deliberate mutations each fail a check: no cache, cache ignoring the generation, no reset in
  `socket()`, `close` keeping the timestamp flag, a single-copy latch, `getsockopt` on every send,
  an off-by-one CIDR mask, `learn` not bumping the generation, a bucket without carry-over, and a
  `dup2` that resets on failure.
- Apple-only code: `zig cc -target aarch64-macos` compiles the injected build with no warnings, and
  its `__interpose` section holds 27 tuples (21 before, plus the six lifecycle hooks). A strict
  arm64 macOS dylib link resolves every libSystem symbol, including `close$NOCANCEL`,
  `guarded_close_np`, `sendmsg_x` and `connectx`. Only the Network, SystemConfiguration and
  libdns_services symbols are left for the real build's frameworks and `.tbd`.

### On a Mac (not verified here)

1. `cargo nextest run -p sim-netshape`, which runs the C tests with Apple clang, then the benchmark
   above, both on the host.
2. Build and boot-inject: `simee serve &`, then
   `simee call network_shape '{"device":"<udid>","inject":"boot","preset":"none"}'`. This builds the
   dylib with the iOS-simulator SDK (a link failure would name the symbol; `close$NOCANCEL` and
   `guarded_close_np` are new). The device reboots because SpringBoard maps the old build.
   `simee call network_state '{"device":"<udid>"}'` should show `"boot_injected": true`.
3. SpringBoard stability: use the device for a few minutes (home screen, Safari, Settings), then
   check that SpringBoard's PID hasn't changed (`simee call processes '{"device":"<udid>"}'`) and
   that there are no new `SpringBoard*.ips` crash reports.
4. Shaping unchanged: run `docs/research/network-shaping-matrix.py` with the `boot` mode as in
   [network-shaping.md](network-shaping.md) (probe `network-shaping-bench.m`, servers
   `network-shaping-servers.py`). Compare against that doc's `boot` column: offline errors,
   +500 ms per round trip, 1 Mbps, loss, and both rule rows.
5. Overhead: with `preset: none`, `raw TCP (BSD)` `echo256k` throughput and `avg_rtt`, and `raw UDP`
   `avg_rtt`, should match an uninjected run within noise. With `latency_ms: 500` the TCP numbers
   should still read about 508 ms. `sample <pid> 5` of a busy shaped app should show no time in
   `getsockopt` or `pthread_mutex_lock` under `my_send`/`my_recv`.
6. Kernel cross-check (needs sudo): `scripts/pf-shape.sh apply --ports 41000:41999 --latency-ms 200`
   with `port_base: 41000` in the profile. Shaped sockets still bind into the range.
