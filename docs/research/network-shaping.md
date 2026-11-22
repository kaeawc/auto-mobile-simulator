# Network shaping for simulated iOS devices without root

What sim-rs can do to a simulated device's network with no root, no MDM profile and no network
extension, measured per protocol; plus the kernel (pf/dummynet) and network-extension paths for
comparison.

- Host: macOS 26.6, Xcode 26.6, iOS 26.5 runtime, Apple silicon. The host was heavily loaded (load
  average 400–800) during measurement, so treat small timings as noisy.
- Bench: an in-device client (`network-shaping-bench.m`) against host echo servers on the LAN address
  (`network-shaping-servers.py`: TCP, UDP, WebSocket, HTTP) and public endpoints (example.com,
  cloudflare-quic.com for HTTP/3, ws.postman-echo.com for WSS). `network-shaping-matrix.py` drives
  every probe under every mechanism and profile through the sim-rs socket API.

## Summary

| Mechanism | How | Covers | Doesn't cover | Needs |
| --- | --- | --- | --- | --- |
| **Boot injection** (recommended) | sim-rs reboots the device with its socket/DNS shaper (`netshape.dylib`) in launchd's environment (`network_shape {inject:"boot"}`, `sim-rs boot --shaped`); every process loads it | Everything tested: HTTP/1.1, HTTP/2, HTTP/3 (QUIC), WS/WSS, raw TCP/UDP, Network.framework TCP/UDP, DNS, **WebKit** (Safari verified), system daemons. Per-host rules by hostname or CIDR. Live changes (profile file re-read within 250 ms) | Kernel-exact queueing; HTTP/2 setup latency is overcounted (below) | A reboot of the device (about 10 s warm) |
| **Launch injection** | The shaper in one app's launch environment (`launch {shaped:true}`, `network_env` for `SIMCTL_CHILD_*`) | The app's own sockets and DNS (URLSession, Network.framework, BSD sockets) | WebKit (separate ExtensionKit process), system daemons acting for the app | Nothing |
| **launchd setenv** | `launchctl setenv DYLD_INSERT_LIBRARIES` inside the device (`inject:true`) | Processes started afterwards | Running processes; ExtensionKit extensions (sanitized environment) | Nothing |
| **Device proxy** | A shaping HTTP/SOCKS proxy in the sim-rs process, set in the device's own `configd_sim` (`proxy:true`) | Proxy-aware HTTPS/WSS to public hosts, including WebKit; per-host log (`network_flows`) without decryption | Private/LAN addresses (CFNetwork bypasses the proxy), QUIC (forced down to HTTP/2), raw TCP/UDP and Network.framework (SOCKS is ignored), DNS | Nothing |
| **pf + dummynet** | `scripts/pf-shape.sh` pipes for the shaper's source-port range | Kernel-exact delay, bandwidth and random loss for TCP, UDP and QUIC | — | sudo (not run here) |
| **Network extension** | auto-mobile's `NEFilterDataProvider` system extension (`ios/network-filter`) | Per-flow offline; latency/bandwidth by pausing flows | Precise shaping; headless CI | Developer ID signing, user approval in System Settings or an MDM profile |

**Verdict for headless CI with no root and no MDM:** boot injection. It's the only mechanism that
reaches every process, WebKit included, and it needs nothing installed. When exact TCP timing
matters, add pf/dummynet on runners that allow sudo (hosted macOS runners do).

## How the shaper works

`crates/sim-netshape/native/netshape.c` is interposed into iOS-simulator processes with
`DYLD_INSERT_LIBRARIES`. It shapes only sockets whose peer isn't loopback, so local control channels
(XCTest runners, sim-rs) are untouched.

- **Offline:** `connect`/`connectx` fail with `ENETUNREACH`; DNS lookups fail; datagram sends fail;
  `nw_path_get_status` reports *unsatisfied* and `SCNetworkReachabilityGetFlags` reports not
  reachable, so app code using `NWPathMonitor` sees no network (verified). URLSession itself reports
  -1003/-1004 rather than airplane mode's -1009, because its internal path evaluation doesn't go
  through the exported getter.
- **Latency:** TCP pays one round trip at connect (blocking or not) and one per request/response
  turnaround. UDP is a true delay line: each datagram is released at its kernel arrival time
  (`SO_TIMESTAMP`) plus the latency, so a burst arrives together and request/response pays exactly
  one round trip; sends are never delayed, so QUIC's ACKs flow and it stays on HTTP/3. DNS lookups
  pay one round trip.
- **Bandwidth:** token buckets per direction, shared across the process's shaped sockets; nested
  libSystem calls (`recv` → `recvfrom`) are counted once.
- **Loss:** TCP never loses data, it retransmits, so a lost SYN costs the retransmission timeout
  (1 s, doubling; six in a row time out) and a lost segment stalls the stream about 200 ms plus a
  round trip. UDP datagrams are dropped (whole batches for `sendmsg_x`).
- **Per-host rules:** `rule <pattern> key=value…` with CIDR, address, hostname suffix or `*`. Names
  are learned from `getaddrinfo`, `DNSServiceGetAddrInfo(Ex)` (Network.framework and URLSession
  resolve through the `Ex` variant) and, as a fallback, the TLS SNI or HTTP `Host` of a socket's
  first bytes. Resolver names arrive as FQDNs (`example.com.`) and are normalized.
- **Source ports:** with `port_base`, shaped sockets bind to a fixed source-port range so pf can
  target exactly those flows.

## Limits found

- **HTTP/2 + TLS setup latency is overcounted** (example.com at 500 ms RTT: about 3.1–3.3 s
  measured; about 4 RTTs are needed, but small control frames sent mid-response can add round trips
  a real link wouldn't). TCP sockets expose no segment arrival times, so request/response traffic is
  exact and pipelined traffic is approximate. pf/dummynet is exact. Two heuristics were tried and
  rejected: a minimum request size broke small-message protocols (WebSocket echo), and deferring the
  handshake to the first receive made Network.framework report *ready* a round trip early.
- **Hostname offline rules fail fast only with DNS learning.** Before the `DNSServiceGetAddrInfoEx`
  hook, the name was only known from SNI after connecting; URLSession then retried every address
  for about 15 s whatever errno was returned.
- **QUIC can't carry hostname rules learned from SNI** (the Initial is encrypted). Rules apply to
  QUIC through DNS learning or CIDR rules, which works.
- **Don't combine the proxy with injection**: latencies stack (HTTP/2 at 500 ms took 13 s).
- **The proxy sends private addresses direct**: CFNetwork bypasses the configured proxy for the LAN
  host, and Network.framework ignored the advertised SOCKS proxy for raw TCP.
- **Rebuilding the shaper** changes its cached path; `network_shape {inject:"boot"}` checks that
  SpringBoard maps the current build and reboots otherwise.
- **Measured overhead** is negligible for unshaped devices (the base case matches uninjected timing
  within noise), and the shaper ran in about 120 processes per device without issues.

## Kernel shaping: pf + dummynet

`scripts/pf-shape.sh apply --ports 41000:41999 --latency-ms 200 --down-kbps 1000 --up-kbps 500
--loss-pct 5` (as root) configures two dummynet pipes and loads rules into the `com.apple/sim-rs`
anchor that macOS's stock `/etc/pf.conf` already includes, so the main ruleset is never flushed; pf
is enabled with a reference token that `remove` releases. Targeting uses source ports: run the
device's shaper with `port_base`/`port_count` (verified: a shaped socket bound to port 41000).
Verified here: script syntax, rule syntax (`pfctl -nvf`), the stock anchors and port binding.
**Not verified:** applying the rules, which needs sudo (a password was required on this host).

## Network extension

auto-mobile's `ios/network-filter` (an `NEFilterDataProvider` system extension) attributes flows to
simulator processes through audit tokens and the `launchd_sim` tree, and applies offline, latency and
bandwidth by pausing and resuming flows. Activation needs Developer ID signing with the content
filter entitlement and approval in System Settings, or an MDM profile with the system-extension and
web-content-filter payloads. That rules it out for headless CI without MDM. A content filter also
can't shape precisely (it pauses flows instead of pacing bytes). sim-rs's boot injection covers the
same surface without any of that.

## How auto-mobile would drive it

- `networkCondition` on a sim-rs device: call `network_shape` over the sim-rs socket with
  `inject:"boot"` once per session (one device reboot), then change profiles live with
  `network_shape {preset | fields | rules}`; `network_clear` to reset.
- Without sim-rs owning the device: launch the app with `SIMCTL_CHILD_DYLD_INSERT_LIBRARIES` and
  `SIMCTL_CHILD_SIM_RS_NETSHAPE` from `network_env` (app-level coverage, no WebKit).
- Verification: `connections` / `traffic` show per-process sockets and bytes without any extension;
  `network_flows` lists the hosts a device contacted through the proxy.

## Results

Four mechanisms; `inject` is launch injection of the probe process, `boot` is a boot-injected device.
Timings are wall-clock seconds; `avg_rtt` is per echo; `echo256k` sends and receives 256 KB on one
thread, so a 1 Mbps link in each direction shows about 470 kbps (each direction alone measured
962/947 kbps).

#### none

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | ok 0.10s http/1.1 | ok 0.02s http/1.1 | ok 0.01s http/1.1 | ok 0.01s http/1.1 |
| https h2 (example.com) | ok 0.26s h2 | ok 0.10s h2(via-proxy) | ok 0.06s h2(via-proxy) | ok 0.06s h2 |
| https h3 (QUIC) | ok 0.06s h3 | ok 0.09s h2(via-proxy) | ok 0.09s h2(via-proxy) | ok 0.06s h3 |
| websocket ws (local) | ok echoes=4/4 avg_rtt=0ms total=0.02s | ok echoes=4/4 avg_rtt=0ms total=0.02s | ok echoes=4/4 avg_rtt=1ms total=0.02s | ok echoes=4/4 avg_rtt=4ms total=0.03s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=35ms total=0.29s | ok echoes=4/4 avg_rtt=93ms total=0.79s | ok echoes=4/4 avg_rtt=61ms total=0.36s | ok echoes=4/4 avg_rtt=60ms total=0.40s |
| raw TCP (BSD) | ok connect=1ms avg_rtt=1ms echo256k=0.03s (76340 kbps) | ok connect=0ms avg_rtt=0ms echo256k=0.00s (1301005 kbps) | ok connect=0ms avg_rtt=1ms echo256k=0.00s (1828329 kbps) | ok connect=1ms avg_rtt=0ms echo256k=0.01s (371253 kbps) |
| raw UDP (BSD) | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=3ms |
| TCP (Network.framework) | ok ready=7ms replies=4/4 avg_rtt=0ms | ok ready=8ms replies=4/4 avg_rtt=2ms | ok ready=4ms replies=4/4 avg_rtt=0ms | ok ready=4ms replies=4/4 avg_rtt=3ms |
| UDP (Network.framework) | ok ready=5ms replies=6/6 avg_rtt=0ms | ok ready=5ms replies=6/6 avg_rtt=4ms | ok ready=2ms replies=6/6 avg_rtt=0ms | ok ready=7ms replies=6/6 avg_rtt=2ms |
| DNS (getaddrinfo) | ok  188ms | ok  115ms | ok  112ms | ok  124ms |

#### offline

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | err -1004 0.01s | ok 0.02s http/1.1 | err -1004 0.01s | err -1004 0.01s |
| https h2 (example.com) | err -1003 0.01s | err -1004 0.02s | err -1004 0.02s | err -1003 0.01s |
| https h3 (QUIC) | err -1003 0.02s | err -1004 0.02s | err -1004 0.02s | err -1003 0.02s |
| websocket ws (local) | err no echo 0.01s | ok echoes=4/4 avg_rtt=0ms total=0.02s | err no echo 0.01s | err no echo 0.01s |
| websocket wss (public) | err no echo 0.01s | err no echo 0.02s | err no echo 0.02s | err no echo 0.02s |
| raw TCP (BSD) | err connect errno=51 0.00s | ok connect=1ms avg_rtt=4ms echo256k=0.01s (374637 kbps) | err connect errno=51 0.00s | err connect errno=51 0.00s |
| raw UDP (BSD) | err sent=10 send_errors=10 replies=0 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=6ms | err sent=10 send_errors=10 replies=0 avg_rtt=0ms | err sent=10 send_errors=10 replies=0 avg_rtt=0ms |
| TCP (Network.framework) | err not ready 0.00s | ok ready=5ms replies=4/4 avg_rtt=0ms | err not ready 0.00s | err not ready 0.00s |
| UDP (Network.framework) | err not ready 0.00s | ok ready=5ms replies=6/6 avg_rtt=5ms | err not ready 0.00s | err not ready 0.00s |
| DNS (getaddrinfo) | err NXDOMAIN 0ms | ok  172ms | err NXDOMAIN 0ms | err NXDOMAIN 0ms |

#### latency 500 ms

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | ok 1.04s http/1.1 | ok 0.01s http/1.1 | ok 1.03s http/1.1 | ok 1.04s http/1.1 |
| https h2 (example.com) | ok 3.34s h2 | ok 2.11s h2(via-proxy) | ok 13.43s h2(via-proxy) | ok 3.10s h2 |
| https h3 (QUIC) | ok 5.15s h3 | ok 2.62s h2(via-proxy) | ok 13.74s h2(via-proxy) | ok 4.68s h3 |
| websocket ws (local) | ok echoes=4/4 avg_rtt=509ms total=3.06s | ok echoes=4/4 avg_rtt=1ms total=0.03s | ok echoes=4/4 avg_rtt=510ms total=3.08s | ok echoes=4/4 avg_rtt=508ms total=3.06s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=548ms total=5.98s | ok echoes=4/4 avg_rtt=573ms total=4.50s | err no echo 10.01s | ok echoes=4/4 avg_rtt=543ms total=6.00s |
| raw TCP (BSD) | ok connect=507ms avg_rtt=517ms echo256k=8.14s (258 kbps) | ok connect=0ms avg_rtt=3ms echo256k=0.01s (188575 kbps) | ok connect=507ms avg_rtt=507ms echo256k=8.16s (257 kbps) | ok connect=508ms avg_rtt=509ms echo256k=8.14s (257 kbps) |
| raw UDP (BSD) | ok sent=10 send_errors=0 replies=10 avg_rtt=513ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=511ms | ok sent=10 send_errors=0 replies=10 avg_rtt=510ms |
| TCP (Network.framework) | ok ready=513ms replies=4/4 avg_rtt=508ms | ok ready=4ms replies=4/4 avg_rtt=0ms | ok ready=514ms replies=4/4 avg_rtt=511ms | ok ready=514ms replies=4/4 avg_rtt=509ms |
| UDP (Network.framework) | ok ready=2ms replies=6/6 avg_rtt=509ms | ok ready=3ms replies=6/6 avg_rtt=0ms | ok ready=4ms replies=6/6 avg_rtt=515ms | ok ready=3ms replies=6/6 avg_rtt=507ms |
| DNS (getaddrinfo) | ok  664ms | ok  155ms | ok  645ms | ok  657ms |

#### 1 Mbps

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | ok 1.64s http/1.1 | ok 0.02s http/1.1 | ok 1.64s http/1.1 | ok 1.63s http/1.1 |
| https h2 (example.com) | ok 0.21s h2 | ok 0.16s h2(via-proxy) | ok 0.67s h2(via-proxy) | ok 0.49s h2 |
| https h3 (QUIC) | ok 0.52s h2 | ok 0.41s h2(via-proxy) | ok 0.50s h2(via-proxy) | ok 0.62s h3 |
| websocket ws (local) | ok echoes=4/4 avg_rtt=0ms total=0.02s | ok echoes=4/4 avg_rtt=0ms total=0.01s | ok echoes=4/4 avg_rtt=1ms total=0.02s | ok echoes=4/4 avg_rtt=4ms total=0.04s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=32ms total=0.40s | ok echoes=4/4 avg_rtt=29ms total=1.48s | ok echoes=4/4 avg_rtt=116ms total=1.10s | ok echoes=4/4 avg_rtt=288ms total=1.79s |
| raw TCP (BSD) | ok connect=0ms avg_rtt=7ms echo256k=4.37s (480 kbps) | ok connect=0ms avg_rtt=2ms echo256k=0.02s (132825 kbps) | ok connect=0ms avg_rtt=4ms echo256k=4.47s (469 kbps) | ok connect=0ms avg_rtt=14ms echo256k=4.44s (472 kbps) |
| raw UDP (BSD) | ok sent=10 send_errors=0 replies=10 avg_rtt=2ms | ok sent=10 send_errors=0 replies=10 avg_rtt=2ms | ok sent=10 send_errors=0 replies=10 avg_rtt=2ms | ok sent=10 send_errors=0 replies=10 avg_rtt=7ms |
| TCP (Network.framework) | ok ready=4ms replies=4/4 avg_rtt=1ms | ok ready=3ms replies=4/4 avg_rtt=7ms | ok ready=3ms replies=4/4 avg_rtt=0ms | ok ready=3ms replies=4/4 avg_rtt=1ms |
| UDP (Network.framework) | ok ready=4ms replies=6/6 avg_rtt=1ms | ok ready=2ms replies=6/6 avg_rtt=3ms | ok ready=3ms replies=6/6 avg_rtt=11ms | ok ready=3ms replies=6/6 avg_rtt=2ms |
| DNS (getaddrinfo) | ok  118ms | ok  113ms | ok  163ms | ok  171ms |

#### 30% loss

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | ok 0.43s http/1.1 | ok 0.01s http/1.1 | ok 0.22s http/1.1 | ok 0.23s http/1.1 |
| https h2 (example.com) | ok 15.30s h2 | ok 0.31s h2(via-proxy) | ok 2.03s h2(via-proxy) | ok 0.64s h2 |
| https h3 (QUIC) | ok 0.25s h3 | ok 10.09s h2(via-proxy) | ok 12.78s h2(via-proxy) | ok 9.57s h2 |
| websocket ws (local) | ok echoes=4/4 avg_rtt=210ms total=1.87s | ok echoes=4/4 avg_rtt=0ms total=0.01s | ok echoes=4/4 avg_rtt=138ms total=1.85s | ok echoes=4/4 avg_rtt=72ms total=3.45s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=60ms total=1.74s | ok echoes=4/4 avg_rtt=43ms total=0.47s | err no echo 10.00s | ok echoes=4/4 avg_rtt=213ms total=1.48s |
| raw TCP (BSD) | ok connect=0ms avg_rtt=156ms echo256k=1.67s (1256 kbps) | ok connect=0ms avg_rtt=5ms echo256k=0.04s (54184 kbps) | ok connect=0ms avg_rtt=53ms echo256k=1.87s (1123 kbps) | ok connect=1011ms avg_rtt=207ms echo256k=2.71s (773 kbps) |
| raw UDP (BSD) | ok sent=10 send_errors=0 replies=9 avg_rtt=1ms | ok sent=10 send_errors=0 replies=10 avg_rtt=1ms | ok sent=10 send_errors=0 replies=10 avg_rtt=1ms | ok sent=10 send_errors=0 replies=5 avg_rtt=9ms |
| TCP (Network.framework) | ok ready=5ms replies=4/4 avg_rtt=157ms | ok ready=4ms replies=4/4 avg_rtt=1ms | ok ready=3ms replies=4/4 avg_rtt=53ms | ok ready=1007ms replies=4/4 avg_rtt=56ms |
| UDP (Network.framework) | err ready=5ms replies=0/6 avg_rtt=0ms | ok ready=3ms replies=6/6 avg_rtt=0ms | ok ready=2ms replies=2/6 avg_rtt=0ms | ok ready=2ms replies=1/6 avg_rtt=1ms |
| DNS (getaddrinfo) | ok  155ms | ok  166ms | ok  263ms | ok  116ms |

#### rule 192.168.1.5 offline

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | err -1004 0.01s | ok 0.01s http/1.1 | err -1004 0.01s | err -1004 0.01s |
| https h2 (example.com) | ok 0.20s h2 | ok 0.06s h2(via-proxy) | ok 0.21s h2(via-proxy) | ok 0.61s h2 |
| https h3 (QUIC) | ok 0.09s h3 | ok 0.08s h2(via-proxy) | ok 0.38s h2(via-proxy) | ok 0.21s h3 |
| websocket ws (local) | err no echo 0.01s | ok echoes=4/4 avg_rtt=1ms total=0.02s | err no echo 0.01s | err no echo 0.01s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=30ms total=0.30s | ok echoes=4/4 avg_rtt=66ms total=0.39s | ok echoes=4/4 avg_rtt=221ms total=1.35s | ok echoes=4/4 avg_rtt=95ms total=6.05s |
| raw TCP (BSD) | err connect errno=51 0.00s | ok connect=0ms avg_rtt=0ms echo256k=0.00s (738423 kbps) | err connect errno=51 0.00s | err connect errno=51 0.00s |
| raw UDP (BSD) | err sent=10 send_errors=10 replies=0 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | err sent=10 send_errors=10 replies=0 avg_rtt=0ms | err sent=10 send_errors=10 replies=0 avg_rtt=0ms |
| TCP (Network.framework) | err not ready 0.00s | ok ready=4ms replies=4/4 avg_rtt=0ms | err not ready 0.00s | err not ready 0.01s |
| UDP (Network.framework) | err not ready 0.00s | ok ready=4ms replies=6/6 avg_rtt=1ms | err not ready 0.00s | err not ready 0.00s |
| DNS (getaddrinfo) | ok  205ms | ok  159ms | ok  128ms | ok  183ms |

#### rule example.com +1500 ms

| Probe | inject | proxy | both | boot |
| --- | --- | --- | --- | --- |
| http (local, 200 KB) | ok 0.01s http/1.1 | ok 0.01s http/1.1 | ok 0.01s http/1.1 | ok 0.02s http/1.1 |
| https h2 (example.com) | ok 10.68s h2 | ok 5.01s h2(via-proxy) | ok 7.60s h2(via-proxy) | ok 14.53s h2 |
| https h3 (QUIC) | ok 0.06s h3 | ok 0.31s h2(via-proxy) | ok 0.62s h2(via-proxy) | ok 0.71s h3 |
| websocket ws (local) | ok echoes=4/4 avg_rtt=6ms total=0.10s | ok echoes=4/4 avg_rtt=3ms total=0.03s | ok echoes=4/4 avg_rtt=0ms total=0.01s | ok echoes=4/4 avg_rtt=1ms total=0.02s |
| websocket wss (public) | ok echoes=4/4 avg_rtt=31ms total=0.42s | ok echoes=4/4 avg_rtt=29ms total=0.35s | ok echoes=4/4 avg_rtt=53ms total=0.32s | ok echoes=4/4 avg_rtt=94ms total=1.24s |
| raw TCP (BSD) | ok connect=0ms avg_rtt=0ms echo256k=0.01s (177889 kbps) | ok connect=0ms avg_rtt=0ms echo256k=0.00s (2372193 kbps) | ok connect=1ms avg_rtt=0ms echo256k=0.00s (1591189 kbps) | ok connect=1ms avg_rtt=6ms echo256k=0.05s (39808 kbps) |
| raw UDP (BSD) | ok sent=10 send_errors=0 replies=10 avg_rtt=1ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=0ms | ok sent=10 send_errors=0 replies=10 avg_rtt=2ms |
| TCP (Network.framework) | ok ready=11ms replies=4/4 avg_rtt=0ms | ok ready=3ms replies=4/4 avg_rtt=0ms | ok ready=3ms replies=4/4 avg_rtt=0ms | ok ready=8ms replies=4/4 avg_rtt=17ms |
| UDP (Network.framework) | ok ready=5ms replies=6/6 avg_rtt=4ms | ok ready=4ms replies=6/6 avg_rtt=2ms | ok ready=2ms replies=6/6 avg_rtt=0ms | ok ready=4ms replies=6/6 avg_rtt=1ms |
| DNS (getaddrinfo) | ok  140ms | ok  147ms | ok  118ms | ok  174ms |
