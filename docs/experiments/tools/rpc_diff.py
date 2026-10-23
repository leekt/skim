"""Differential JSON-RPC harness: the same requests, fixed to one block hash, against skim and a
reference node. Prints per-request match, skim cold (first) and warm (second) latency.

usage: python3 -I rpc_diff.py http://127.0.0.1:8545 <reference-url> [rounds]
"""
import json, sys, time, urllib.request

SKIM, REF = sys.argv[1], sys.argv[2]
ROUNDS = int(sys.argv[3]) if len(sys.argv) > 3 else 1

WETH = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
USDC = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
QUOTER = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
POOL500 = "0x3289680dD4d6C10bb19b899729cda5eEF58AEfF1"
POOL3000 = "0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50"
HISTORY = "0x0000F90827F1C53a10cb7A02335B175320002935"


def pad(h):
    return h.lower().replace("0x", "").rjust(64, "0")


def quote(tin, tout, amt, fee):
    return "0xc6a5026a" + pad(tin) + pad(tout) + pad(hex(amt)) + pad(hex(fee)) + pad("0")


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json", "user-agent": "skim-rpc-diff"})
    t = time.time()
    with urllib.request.urlopen(req, timeout=60) as r:
        out = json.load(r)
    return out, (time.time() - t) * 1e3


def cases(blk):
    b = {"blockHash": blk}
    return [
        ("eth_getBalance pool500", "eth_getBalance", [POOL500, b]),
        ("eth_getTransactionCount quoter", "eth_getTransactionCount", [QUOTER, b]),
        ("eth_getCode WETH", "eth_getCode", [WETH, b]),
        ("eth_getStorageAt pool3000 slot0", "eth_getStorageAt", [POOL3000, "0x0", b]),
        ("eth_getStorageAt history", "eth_getStorageAt", [HISTORY, "0x1", b]),
        ("eth_call WETH.balanceOf(pool500)", "eth_call", [{"to": WETH, "data": "0x70a08231" + pad(POOL500)}, b]),
        ("eth_call USDC(proxy).balanceOf(pool500)", "eth_call", [{"to": USDC, "data": "0x70a08231" + pad(POOL500)}, b]),
        ("eth_call V3 quote 1 WETH fee3000", "eth_call", [{"to": QUOTER, "data": quote(WETH, USDC, 10**18, 3000)}, b]),
        ("eth_call V3 quote 20 WETH fee500", "eth_call", [{"to": QUOTER, "data": quote(WETH, USDC, 20 * 10**18, 500)}, b]),
        ("eth_call V3 quote 5000 USDC fee500", "eth_call", [{"to": QUOTER, "data": quote(USDC, WETH, 5000 * 10**6, 500)}, b]),
        ("eth_call revert (transfer w/o balance)", "eth_call", [{"from": "0x0000000000000000000000000000000000000001", "to": WETH, "data": "0xa9059cbb" + pad("0x2") + pad(hex(10**30))}, b]),
        ("eth_estimateGas WETH transfer", "eth_estimateGas", [{"from": POOL500, "to": WETH, "data": "0xa9059cbb" + pad("0x2") + pad("0x1")}, b]),
    ]


def norm(resp):
    if "result" in resp:
        return ("ok", resp["result"])
    e = resp.get("error", {})
    return ("err", e.get("code"), e.get("data"))


for rnd in range(ROUNDS):
    head, _ = rpc(SKIM, "eth_blockNumber", [])
    n = int(head["result"], 16)
    blk, _ = rpc(SKIM, "eth_getBlockByNumber", [hex(n), False])
    bhash = blk["result"]["hash"]
    ref_blk, _ = rpc(REF, "eth_getBlockByHash", [bhash, False])
    same_header = all(blk["result"][k] == ref_blk["result"][k] for k in ("hash", "parentHash", "stateRoot", "timestamp", "number"))
    print(f"\nblock {n} {bhash}  header fields match reference: {same_header}")
    ok = 0
    for name, method, params in cases(bhash):
        s1, t1 = rpc(SKIM, method, params)
        s2, t2 = rpc(SKIM, method, params)
        r, tr = rpc(REF, method, params)
        match = norm(s1) == norm(r) and norm(s2) == norm(r)
        # estimateGas: nodes differ in search slack; accept within 2%.
        if method == "eth_estimateGas" and "result" in s1 and "result" in r:
            a, c = int(s1["result"], 16), int(r["result"], 16)
            match = abs(a - c) <= max(2, c // 50)
        ok += match
        print(f"  {'OK ' if match else 'BAD'} {name:42} skim cold {t1:7.1f} ms  warm {t2:6.1f} ms  ref {tr:6.1f} ms"
              + ("" if match else f"\n      skim={norm(s1)}\n      ref ={norm(r)}"))
    print(f"  {ok}/{len(cases(bhash))} match")
    if rnd + 1 < ROUNDS:
        time.sleep(24)
