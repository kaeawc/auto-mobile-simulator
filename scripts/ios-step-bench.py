"""Per-step latency of a booted iOS simulator through `simee serve`, over one persistent connection.

    SIMEE_SOCKET=/tmp/s.sock target/release/simee serve &
    python3 scripts/ios-step-bench.py /tmp/s.sock <udid> [N]

Prints the median and p90 of N calls (default 20, after one warm-up) each: tap, screenshot (base64
in the reply), screenshot to a file, and a tap followed by a screenshot (one automation step). A
tap holds 60 ms (the API's default `hold_ms`). It taps (600, 2500), the gap between the two dock
icons of an iPhone 17's home screen, so the taps change nothing."""
import json, os, socket, statistics, sys, time

sock_path, udid = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 20
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
r_ = s.makefile("rb", buffering=1 << 20)
rid = 0


def call(method, **params):
    global rid
    rid += 1
    s.sendall((json.dumps({"id": rid, "method": method, "params": params}) + "\n").encode())
    line = r_.readline()
    r = json.loads(line)
    if not r.get("ok"):
        raise SystemExit(f"{method}: {r}")
    return r["result"], len(line)


def bench(name, fn):
    fn()  # warm: per-device workers, HID client, display
    ts = []
    for _ in range(n):
        t = time.perf_counter()
        fn()
        ts.append((time.perf_counter() - t) * 1000)
    ts.sort()
    print(f"{name}: median {statistics.median(ts):.1f} ms, p90 {ts[int(0.9 * (n - 1))]:.1f} ms, min {ts[0]:.1f} ms (n={n})")


out = os.path.join(os.path.dirname(sock_path), "stepbench.png")
# The gap between the two dock icons (iPhone 17 home screen): taps change nothing.
x, y = 600, 2500
bench("tap", lambda: call("tap", device=udid, x=x, y=y))
bench("screenshot (base64)", lambda: call("screenshot", device=udid))
bench("screenshot (file)", lambda: call("screenshot", device=udid, path=out))
bench("tap + screenshot (file)", lambda: (call("tap", device=udid, x=x, y=y), call("screenshot", device=udid, path=out)))
