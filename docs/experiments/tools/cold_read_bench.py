"""Read latency of a running skim: cold reads of never-read keys, warm repeats, and a contract call.

usage: python3 -I cold_read_bench.py <url> <contract with storage> [n cold] [call data]

Waits until skim follows the chain (and has 3+ SNAP peers, or 10 minutes pass), then:
  cold   n/2 random accounts (eth_getBalance) and n/2 random storage slots of <contract>
         (eth_getStorageAt), sequential, each key never read before;
  warm   200 reads cycling over 10 keys already read;
  call   eth_call <contract> <call data> once cold-ish, then 50 times warm.
Prints p50/p90/p95/p99/max per group and failures by error.data.reason.
"""
import json, os, random, sys, time, urllib.request
from collections import Counter

URL, CONTRACT = sys.argv[1], sys.argv[2]
N = int(sys.argv[3]) if len(sys.argv) > 3 else 600
CALLDATA = sys.argv[4] if len(sys.argv) > 4 else None


def rpc(method, params, timeout=90):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    return json.loads(urllib.request.urlopen(urllib.request.Request(URL, body, {"content-type": "application/json"}), timeout=timeout).read())


def timed(method, params):
    t = time.monotonic()
    try:
        r = rpc(method, params)
    except Exception as e:
        return (time.monotonic() - t) * 1000, f"http: {type(e).__name__}"
    err = r.get("error")
    return (time.monotonic() - t) * 1000, (None if err is None else (err.get("data") or {}).get("reason", str(err.get("code"))))


def report(name, samples):
    ok = sorted(ms for ms, e in samples if e is None)
    fails = Counter(e for _, e in samples if e is not None)
    p = lambda q: ok[min(len(ok) - 1, int(q / 100 * len(ok)))] if ok else float("nan")
    print(f"{name:6} n={len(samples)} ok={len(ok)} p50={p(50):.0f} p90={p(90):.0f} p95={p(95):.0f} p99={p(99):.0f} max={(ok[-1] if ok else float('nan')):.0f} ms"
          + (f"  failures: {dict(fails)}" if fails else ""))


deadline = time.time() + 600
while True:
    try:
        st = rpc("skim_status", [])["result"]
        if st.get("head") and (st.get("snap_peers", 0) >= 3 or time.time() > deadline):
            break
    except Exception:
        pass
    time.sleep(10)
print(f"skim: head {st['head']}, {st['el_peers']} EL peers, {st['snap_peers']} serving state; network {st.get('network')}")

cold = []
for i in range(N):
    if i % 2 == 0:
        cold.append(timed("eth_getBalance", ["0x" + os.urandom(20).hex(), "latest"]))
    else:
        cold.append(timed("eth_getStorageAt", [CONTRACT, hex(random.getrandbits(256)), "latest"]))
report("cold", cold)

keys = [hex(i) for i in range(10)]
for k in keys:
    rpc("eth_getStorageAt", [CONTRACT, k, "latest"])
report("warm", [timed("eth_getStorageAt", [CONTRACT, keys[i % 10], "latest"]) for i in range(200)])

if CALLDATA:
    first = timed("eth_call", [{"to": CONTRACT, "data": CALLDATA}, "latest"])
    print(f"call   first: {first[0]:.0f} ms ({first[1] or 'ok'})")
    report("call", [timed("eth_call", [{"to": CONTRACT, "data": CALLDATA}, "latest"]) for _ in range(50)])
