"""Cold-read latency A/B on one node (same peers, same moment): adaptive vs fixed SNAP hedging.

usage: python3 -I ab.py <n reads> <url>     (skim started with --debug-rpc)
Each read is a never-read key: a random account (eth_getBalance) or a random storage slot of
a large contract (eth_getStorageAt). Per read, the hedging mode is picked at random and set
with skim_debugSetSnapHedge; reads are sequential, so the SNAP requests and hedges each one
spent are attributed to its mode. Prints p50/p90/p95/p99/max and requests/hedges per mode.
"""
import json, os, random, sys, time, urllib.request

N, URL = int(sys.argv[1]), sys.argv[2]
WETH = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"  # Sepolia WETH: a large storage trie


def rpc(method, params, timeout=90):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    return json.loads(urllib.request.urlopen(urllib.request.Request(URL, body, {"content-type": "application/json"}), timeout=timeout).read())


def stats():
    s = rpc("skim_status", [])["result"]["snap_fetch"]["stats"]
    return s["requests"], s["hedges"]


def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(p / 100 * len(v)))]


res = {"adaptive": [], "fixed": []}
cost = {"adaptive": [0, 0], "fixed": [0, 0]}
fails = {"adaptive": 0, "fixed": 0}
for n in range(N):
    mode = random.choice(["adaptive", "fixed"])
    assert rpc("skim_debugSetSnapHedge", [mode]).get("result") == mode
    kind = n % 2
    r0, h0 = stats()
    t = time.monotonic()
    try:
        if kind == 0:
            r = rpc("eth_getBalance", ["0x" + os.urandom(20).hex(), "latest"])
        else:
            r = rpc("eth_getStorageAt", [WETH, hex(random.getrandbits(256)), "latest"])
        ok = "result" in r
    except Exception:
        ok = False
    dt = (time.monotonic() - t) * 1000
    r1, h1 = stats()
    if ok:
        res[mode].append(dt)
    else:
        fails[mode] += 1
    cost[mode][0] += r1 - r0
    cost[mode][1] += h1 - h0
    time.sleep(0.1)
rpc("skim_debugSetSnapHedge", ["adaptive"])
for m, v in res.items():
    req, hed = cost[m]
    print(f"{m:8}: n={len(v)} fail={fails[m]}  p50={pct(v,50):.0f} p90={pct(v,90):.0f} p95={pct(v,95):.0f} p99={pct(v,99):.0f} max={max(v):.0f} ms  "
          f"snap requests/read={req/max(len(v),1):.2f} hedges={hed} ({100*hed/max(req,1):.1f}% of requests)")
