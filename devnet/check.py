"""Compares skim on the devnet with a devnet EL node at the same block hash.

usage: python3 -I devnet/check.py <devnet EL rpc port> [skim rpc port, default 8600]
Checks pre-funded balances, the deposit contract's code, and an eth_call (get_deposit_count).
"""
import json, os, sys, urllib.request

node, skim = int(sys.argv[1]), int(sys.argv[2]) if len(sys.argv) > 2 else 8600
net = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out", "net")

def rpc(port, method, params):
    req = urllib.request.Request(f"http://127.0.0.1:{port}", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=60))

genesis = json.load(open(os.path.join(net, "genesis.json")))
funded = [a for a, v in genesis["alloc"].items() if int(v.get("balance", "0x0"), 16) > 0 and not v.get("code")][:5]
deposit = open(os.path.join(net, "deposit_contract.txt")).read().strip()
head = rpc(skim, "eth_getBlockByNumber", ["latest", False])["result"]
tag = {"blockHash": head["hash"]}
print(f"skim head {int(head['number'], 16)} {head['hash']}")
checks = [("eth_getBalance", [a if a.startswith("0x") else "0x" + a, tag]) for a in funded]
checks += [("eth_getCode", [deposit, tag]), ("eth_call", [{"to": deposit, "data": "0x621fd130"}, tag])]
bad = 0
for method, params in checks:
    a, b = rpc(skim, method, params), rpc(node, method, params)
    same = a.get("result") is not None and a.get("result") == b.get("result")
    bad += not same
    print(f"{'match' if same else 'DIFF '} {method} {str(params[0])[:44]}  skim={str(a.get('result', a.get('error')))[:40]}")
print("all match" if not bad else f"{bad} differ")
sys.exit(1 if bad else 0)
