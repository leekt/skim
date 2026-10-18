"""Memory A/B: several skim builds side by side, same soak config A, same per-block load, no restarts.

usage: [BASE=<port offset>] python3 -I mem_ab.py <out-dir> <minutes> label=binary [label=binary ...]
Writes <out-dir>/<label>/rss.jsonl (every 30 s: rss_mb, head, store stats) and prints a summary.
"""
import json, os, random, subprocess, sys, threading, time, urllib.request

OUT, MINUTES = sys.argv[1], float(sys.argv[2])
BASE = int(os.environ.get("BASE", "0"))
BUILDS = [a.split("=", 1) for a in sys.argv[3:]]
QUOTER = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
POOL = "0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50"
WETH, USDC = "fff9976782d46cc05630d1f6ebab18b2324d6b14", "1c7d4b196cb0c7b01d743fbc6116a902379c7238"
QUOTE_1 = ("0xc6a5026a" + WETH.rjust(64, "0") + USDC.rjust(64, "0") + f"{10**18:x}".rjust(64, "0")
           + f"{3000:x}".rjust(64, "0") + "0" * 64)
CONFIG = f"""
state:
  subscriptions:
    - name: pool
      address: "{POOL}"
      account: true
      storage: ["0x0", "0x1", "0x2", "0x4"]
      retention: {{ mode: archive }}
    - name: quote
      calls: [{{ to: "{QUOTER}", data: "{QUOTE_1}" }}]
      on_block: true
      retention: {{ mode: archive }}
  limits: {{ max_subscribed_keys: 60 }}
predictors:
  uniswap_v3:
    pools: ["{POOL}"]
    prefetch_budget: 16
"""


def rpc(url, method, params, timeout=60):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    except Exception as e:
        return {"error": {"message": str(e)}}


def random_quote(rnd):
    weth_in = rnd.random() < 0.5
    amt = int(10 ** (14 + 5 * rnd.random())) if weth_in else int(10 ** (3 + 7 * rnd.random()))
    tin, tout = (WETH, USDC) if weth_in else (USDC, WETH)
    return {"to": QUOTER, "data": "0xc6a5026a" + tin.rjust(64, "0") + tout.rjust(64, "0")
            + f"{amt:x}".rjust(64, "0") + f"{3000:x}".rjust(64, "0") + "0" * 64}


def blockhash_probe(n):
    return {"data": "0x67" + f"{n:016x}" + "40" + "5f52" + "6020" + "5ff3"}


def drive(i, label, binary, stop, results):
    d = os.path.join(OUT, label)
    os.makedirs(d, exist_ok=True)
    cfg = os.path.join(d, "skim.yaml")
    open(cfg, "w").write(CONFIG)
    port = 8700 + BASE + 10 * i
    url = f"http://127.0.0.1:{port}"
    log = open(os.path.join(d, "skim.log"), "w")
    p = subprocess.Popen([binary, "run", "--config", cfg, "--rpc", f"127.0.0.1:{port}", "--cl-port", str(9400 + BASE + 10 * i),
                          "--el-port", str(30700 + BASE + 10 * i), "--archive", os.path.join(d, "a.redb"), "--data-dir", os.path.join(d, "data")],
                         stdout=log, stderr=log, env=dict(os.environ, RUST_LOG="info"))
    out = open(os.path.join(d, "rss.jsonl"), "w")
    last, next_sample, rss = None, 0, []
    while not stop.is_set():
        now = time.time()
        if now >= next_sample:
            next_sample = now + 30
            kb = subprocess.run(["ps", "-o", "rss=", "-p", str(p.pid)], capture_output=True, text=True).stdout.strip()
            st = rpc(url, "skim_status", [], timeout=10).get("result") or {}
            row = {"t": now, "rss_mb": int(kb) / 1024 if kb else None, "head": (st.get("head")), "store": st.get("store"), "el_peers": st.get("el_peers")}
            out.write(json.dumps(row) + "\n"); out.flush()
            if row["rss_mb"]:
                rss.append(row["rss_mb"])
        head = rpc(url, "eth_getBlockByNumber", ["latest", True], timeout=10).get("result")
        if not head or head["hash"] == last:
            time.sleep(1)
            continue
        last = head["hash"]
        n = int(head["number"], 16)
        rnd = random.Random(n)  # the same load for every build at the same block
        tag = {"blockHash": head["hash"]}
        for _ in range(3):
            rpc(url, "eth_call", [random_quote(rnd), tag])
        rpc(url, "eth_call", [{"to": QUOTER, "data": QUOTE_1}, tag])
        for s in ("0x0", "0x1", "0x2", "0x4"):
            rpc(url, "eth_getStorageAt", [POOL, s, tag])
        rpc(url, "eth_getStorageAt", [POOL, hex(rnd.randrange(8, 4000)), tag])
        rpc(url, "eth_call", [blockhash_probe(n - rnd.randint(1, 256)), tag])
        senders = sorted({t["from"] for t in head.get("transactions", []) if isinstance(t, dict)})
        for a in rnd.sample(senders, min(3, len(senders))):
            rpc(url, "eth_getBalance", [a, tag])
        if n % 4 == 0:  # archived reads, older than the in-memory window
            rpc(url, "eth_getStorageAt", [POOL, "0x0", hex(n - rnd.randint(40, 200))])
    p.terminate()
    try:
        p.wait(20)
    except subprocess.TimeoutExpired:
        p.kill()
    results[label] = rss


stop, results = threading.Event(), {}
threads = [threading.Thread(target=drive, args=(i, l, b, stop, results)) for i, (l, b) in enumerate(BUILDS)]
for t in threads:
    t.start()
time.sleep(MINUTES * 60)
stop.set()
for t in threads:
    t.join()
for label, r in results.items():
    s = sorted(r)
    if s:
        print(f"{label}: samples={len(s)} median={s[len(s)//2]:.0f} q3={s[3*len(s)//4]:.0f} max={s[-1]:.0f} last={r[-1]:.0f} MB")
