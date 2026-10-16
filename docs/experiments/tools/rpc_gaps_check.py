"""Compare skim's block-data, fee, access-list, proof, tracer and simulate extensions with a
reference node at the same block.

usage: python3 -I rpc_gaps_check.py <skim-url> <reference-url> [geth-reference-url]

The optional geth reference is used for eth_feeHistory rewards and eth_createAccessList, where
clients differ (Nethermind computes rewards differently and returns no access list here).
"""
import json, sys, urllib.request

SKIM, REF = sys.argv[1], sys.argv[2]
GETH = sys.argv[3] if len(sys.argv) > 3 else REF
WETH = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
QUOTER = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
X = "0x00000000000000000000000000000000000a11ce"
Y = "0x0000000000000000000000000000000000000b0b"
QUOTE = ("0xc6a5026a" + WETH[2:].lower().rjust(64, "0") + "1c7d4b196cb0c7b01d743fbc6116a902379c7238".rjust(64, "0")
         + f"{10**18:x}".rjust(64, "0") + f"{3000:x}".rjust(64, "0") + "0" * 64)


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.load(r)


def diff(a, b, path=""):
    if isinstance(a, dict) and isinstance(b, dict):
        out = []
        for k in sorted(a.keys() & b.keys()):
            out += diff(a[k], b[k], f"{path}.{k}")
        return out
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            return [f"{path}: len {len(a)} vs {len(b)}"]
        return [d for i, (x, y) in enumerate(zip(a, b)) for d in diff(x, y, f"{path}[{i}]")]
    if isinstance(a, str) and isinstance(b, str):
        return [] if a.lower() == b.lower() else [f"{path}: {a[:66]} vs {b[:66]}"]
    return [] if a == b else [f"{path}: {a} vs {b}"]


n = int(rpc(SKIM, "eth_blockNumber", [])["result"], 16) - 2
tag = hex(n)
results = []


def check(name, method, params, pick=lambda r: r, ignore=(), ref=None):
    s, r = rpc(SKIM, method, params), rpc(ref or REF, method, params)
    if "error" in s or "error" in r:
        results.append((name, False, [f"skim: {s.get('error')}", f"ref: {r.get('error')}"]))
        return
    a, b = pick(s["result"]), pick(r["result"])
    d = [x for x in diff(a, b) if not any(i in x for i in ignore)]
    results.append((name, not d, d[:6]))


TX_FIELDS = ("hash", "from", "to", "nonce", "value", "input", "gas", "type", "blockNumber", "blockHash", "transactionIndex",
             "maxFeePerGas", "maxPriorityFeePerGas", "gasPrice", "chainId", "r", "s", "v", "yParity", "accessList")
check("eth_getBlockByNumber (full txs)", "eth_getBlockByNumber", [tag, True],
      pick=lambda b: [{k: t.get(k) for k in TX_FIELDS} for t in b["transactions"]])
check("eth_getBlockByNumber (hashes)", "eth_getBlockByNumber", [tag, False], pick=lambda b: b["transactions"])
check("eth_getBlockReceipts", "eth_getBlockReceipts", [tag])
check("eth_getTransactionByBlockNumberAndIndex", "eth_getTransactionByBlockNumberAndIndex", [tag, "0x0"],
      pick=lambda t: {k: t.get(k) for k in TX_FIELDS} if t else t)
check("eth_getBlockTransactionCountByNumber", "eth_getBlockTransactionCountByNumber", [tag])
check("eth_feeHistory (10 blocks, 10/50/90)", "eth_feeHistory", ["0xa", tag, [10, 50, 90]],
      pick=lambda f: {k: f.get(k) for k in ("oldestBlock", "baseFeePerGas", "gasUsedRatio", "reward")}, ref=GETH)
check("eth_createAccessList (V3 quote)", "eth_createAccessList", [{"to": QUOTER, "data": QUOTE, "gas": "0x989680"}, tag], ref=GETH)
check("eth_getProof (WETH, 3 slots)", "eth_getProof", [WETH, ["0x0", "0x2", "0x" + "11" * 32], tag])
check("eth_getProof (absent account)", "eth_getProof", [Y, ["0x1"], tag])
check("debug_traceCall 4byteTracer", "debug_traceCall", [{"to": QUOTER, "data": QUOTE, "gas": "0x989680"}, tag, {"tracer": "4byteTracer"}])
check("debug_traceCall muxTracer", "debug_traceCall",
      [{"to": QUOTER, "data": QUOTE, "gas": "0x989680"}, tag, {"tracer": "muxTracer", "tracerConfig": {"callTracer": {}, "4byteTracer": {}}}],
      ignore=("executionGasUsed", "regularGasUsed"))
check("debug_traceCall blockOverrides (NUMBER)", "debug_traceCall",
      [{"to": Y, "data": "0x", "gas": "0x989680"}, tag,
       {"tracer": "callTracer", "stateOverrides": {Y: {"code": "0x435f5260205ff3"}}, "blockOverrides": {"number": "0x1234"}}],
      ignore=("executionGasUsed", "regularGasUsed"))
sim_tt = {"traceTransfers": True, "blockStateCalls": [{"stateOverrides": {X: {"balance": hex(10**19)}},
          "calls": [{"from": X, "to": WETH, "value": hex(10**18), "data": "0xd0e30db0"},
                    {"from": X, "to": Y, "value": hex(10**17)}]}]}
check("eth_simulateV1 traceTransfers", "eth_simulateV1", [sim_tt, tag],
      pick=lambda r: [[{k: c.get(k) for k in ("status", "returnData", "logs")} for c in b["calls"]] for b in r],
      ignore=("blockHash", "transactionHash", "blockTimestamp"))
fees = {"maxFeePerGas": hex(10**10), "maxPriorityFeePerGas": hex(10**9)}
sim_v = {"validation": True, "blockStateCalls": [{"stateOverrides": {X: {"balance": hex(10**19)}},
         "calls": [dict(fees, **{"from": X, "to": WETH, "value": hex(10**18), "data": "0xd0e30db0"}),
                   dict(fees, **{"from": X, "to": WETH, "data": "0x70a08231" + X[2:].rjust(64, "0")})]},
         {"calls": [dict(fees, **{"from": X, "to": Y, "value": hex(10**17)})]}]}
check("eth_simulateV1 validation", "eth_simulateV1", [sim_v, tag],
      pick=lambda r: [{"baseFeePerGas": b.get("baseFeePerGas"), "calls": [{k: c.get(k) for k in ("status", "returnData", "gasUsed")} for c in b["calls"]]} for b in r])

# Without fees, validation must reject the call with the same error on both.
nofee = {"validation": True, "blockStateCalls": [{"stateOverrides": {X: {"balance": hex(10**19)}}, "calls": [{"from": X, "to": Y, "value": "0x1"}]}]}
s_, r_ = rpc(SKIM, "eth_simulateV1", [nofee, tag]), rpc(REF, "eth_simulateV1", [nofee, tag])
same = s_.get("error", {}).get("code") == r_.get("error", {}).get("code") and "error" in r_
results.append(("eth_simulateV1 validation rejects missing fees", same, [] if same else [f"skim {s_.get('error')}", f"ref {r_.get('error')}"]))

for name, ok, d in results:
    print(f"{'MATCH' if ok else 'DIFF '} {name}")
    for x in d:
        print("      ", x)
print(f"\n{sum(ok for _, ok, _ in results)}/{len(results)} identical at block {n}")
