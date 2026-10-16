"""Compare debug_traceTransaction / debug_traceBlockByNumber between skim and a reference node.

usage: python3 -I replay_check.py <skim-url> <reference-url> [txs]
"""
import json, sys, urllib.request

SKIM, REF = sys.argv[1], sys.argv[2]
NTX = int(sys.argv[3]) if len(sys.argv) > 3 else 12
# EIP-8037 gas-breakdown fields: names differ per client and aren't standardized yet.
IGN = ("executionGasUsed", "regularGasUsed", "stateGasUsed", "gasRefund")


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.load(r)


def diff(a, b, path=""):
    if isinstance(a, dict) and isinstance(b, dict):
        out = []
        for k in sorted(a.keys() | b.keys()):
            if k in IGN:
                continue
            if k not in a or k not in b:
                out.append(f"{path}.{k}: only in {'skim' if k in a else 'ref'}")
            else:
                out += diff(a[k], b[k], f"{path}.{k}")
        return out
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            return [f"{path}: len {len(a)} vs {len(b)}"]
        return [d for i, (x, y) in enumerate(zip(a, b)) for d in diff(x, y, f"{path}[{i}]")]
    if isinstance(a, str) and isinstance(b, str):
        return [] if a.lower() == b.lower() else [f"{path}: {a[:60]} vs {b[:60]}"]
    return [] if a == b else [f"{path}: {a} vs {b}"]


head = int(rpc(SKIM, "eth_blockNumber", [])["result"], 16)
# A recent block with transactions.
for n in range(head - 3, head - 30, -1):
    blk = rpc(REF, "eth_getBlockByNumber", [hex(n), False])["result"]
    if len(blk["transactions"]) >= 3:
        break
txs = blk["transactions"]
step = max(1, len(txs) // NTX)
picked = txs[::step][:NTX]
ok = bad = 0
for t in picked:
    for tracer in ({"tracer": "callTracer"},):
        s, r = rpc(SKIM, "debug_traceTransaction", [t, tracer]), rpc(REF, "debug_traceTransaction", [t, tracer])
        if "error" in s or "error" in r:
            bad += 1
            print("ERR", t[:12], tracer, s.get("error"), r.get("error"))
            continue
        d = diff(s["result"], r["result"])
        ok += not d
        bad += bool(d)
        if d:
            print("DIFF", t[:12], tracer, d[:4])
print(f"block {n}: callTracer on {len(picked)} of {len(txs)} txs: {ok} identical, {bad} different/errors")
t = picked[0]
for tracer in ({"tracer": "prestateTracer"}, {}):
    s, r = rpc(SKIM, "debug_traceTransaction", [t, tracer]), rpc(REF, "debug_traceTransaction", [t, tracer])
    if "error" in s or "error" in r:
        print("ERR", tracer, s.get("error"), r.get("error"))
        continue
    a, b = s["result"], r["result"]
    if not tracer:
        a = {"gas": a.get("gas"), "failed": a.get("failed"), "returnValue": a.get("returnValue"), "steps": len(a.get("structLogs", []))}
        b = {"gas": b.get("gas"), "failed": b.get("failed"), "returnValue": b.get("returnValue"), "steps": len(b.get("structLogs", []))}
    d = diff(a, b)
    print(f"{tracer.get('tracer', 'default')} on {t[:12]}: {'MATCH' if not d else d[:4]}")
s = rpc(SKIM, "debug_traceBlockByNumber", [hex(n), {"tracer": "callTracer"}])
r = rpc(REF, "debug_traceBlockByNumber", [hex(n), {"tracer": "callTracer"}])
if "error" in s or "error" in r:
    print("block trace ERR", s.get("error"), r.get("error"))
else:
    d = diff([x["result"] for x in s["result"]], [x["result"] for x in r["result"]])
    print(f"debug_traceBlockByNumber {n} ({len(s['result'])} txs, callTracer): {'MATCH' if not d else d[:6]}")
