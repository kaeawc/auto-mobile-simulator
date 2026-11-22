# Runs every probe under every shaping mechanism and profile; prints a markdown table.
import json, subprocess, sys, time
SIM, U, T, H = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
def call(method, params):
    params = dict(params, device=U)
    out = subprocess.run([SIM, "call", method, json.dumps(params)], capture_output=True, text=True, timeout=120)
    if out.returncode: raise RuntimeError(out.stderr.strip())
    return json.loads(out.stdout or "null")
ENV = call("network_env", {})
PROBES = [
    ("http (local, 200 KB)", ["http", f"http://{H}:18081/bytes/200000"]),
    ("https h2 (example.com)", ["http", "https://example.com/"]),
    ("https h3 (QUIC)", ["h3", "https://cloudflare-quic.com/"]),
    ("websocket ws (local)", ["ws", f"ws://{H}:18082/", "4"]),
    ("websocket wss (public)", ["ws", "wss://ws.postman-echo.com/raw", "4"]),
    ("raw TCP (BSD)", ["tcp", H, "18083", "4"]),
    ("raw UDP (BSD)", ["udp", H, "18084", "10"]),
    ("TCP (Network.framework)", ["nwtcp", H, "18083", "4"]),
    ("UDP (Network.framework)", ["nwudp", H, "18084", "6"]),
    ("DNS (getaddrinfo)", ["dns", "dynamic"]),
]
PROFILES = [
    ("none", {"preset": "none"}),
    ("offline", {"preset": "offline"}),
    ("latency 500 ms", {"preset": "none", "latency_ms": 500}),
    ("1 Mbps", {"preset": "none", "down_kbps": 1000, "up_kbps": 1000}),
    ("30% loss", {"preset": "none", "loss_pct": 30}),
    (f"rule {H} offline", {"preset": "none", "rules": [{"pattern": H, "offline": True}]}),
    ("rule example.com +1500 ms", {"preset": "none", "rules": [{"pattern": "example.com", "latency_ms": 1500}]}),
]
ALL_MODES = [("inject", {"proxy": False}, True), ("proxy", {"proxy": True}, False), ("both", {"proxy": True}, True), ("boot", {"proxy": False}, False)]
WANT = sys.argv[5].split(",") if len(sys.argv) > 5 else ["inject", "proxy", "both"]
MODES = [m for m in ALL_MODES if m[0] in WANT]
def run(args, inject):
    cmd = [SIM, "exec", U]
    if inject:
        for k, v in ENV.items(): cmd += ["--env", f"{k}={v}"]
    cmd += [T] + args
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=90).stdout.strip().splitlines()
        return out[-1] if out else "(no output)"
    except subprocess.TimeoutExpired:
        return "TIMEOUT 90s"
results = {}
for mname, mparams, inject in MODES:
    for pname, pparams in PROFILES:
        call("network_clear", {})
        call("network_shape", dict(pparams, **mparams))
        time.sleep(0.6)
        for label, args in PROBES:
            if args[0] == "dns": args = ["dns", f"p{int(time.time()*1000)}.1.2.3.4.nip.io"]
            r = run(args, inject)
            results[(mname, pname, label)] = r
            print(f"{mname:6} | {pname:28} | {label:26} | {r}", flush=True)
call("network_clear", {})
