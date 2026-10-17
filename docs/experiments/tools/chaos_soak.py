"""Long-run correctness soak: skim must answer right or refuse, under restarts and key-set churn.

usage: REF_URL=<reference node> [SKIM_EXTRA_ARGS="--trusted-peer enode://…"] python3 -I chaos_soak.py <skim-binary> <out-dir> [minutes]

Every block, the same requests go to skim and to the reference at the same block hash:
  - random Uniswap V3 quotes (cold misses, prediction)
  - subscribed and unsubscribed storage slots
  - BLOCKHASH(n - k) for random k in 1..256 (contract-creation probe)
  - GASPRICE with an explicit gasPrice (request fields must be honoured)
  - archived slots at older blocks (the in-memory window is short, so these come from disk)
Each answer is MATCH, REFUSED (skim returned an error, e.g. -32002), or WRONG (skim returned a
value the reference contradicts). WRONG is the failure this soak exists to catch.

  - balances of the current block's senders (untracked: answered from the recent BAL index)
  - restart_archive: an archived subscription created at each restart for an account that just
    transacted, read back at blocks older than the in-memory window, only from where skim reports
    its coverage starting (skim_archiveCoverage), so a refusal inside coverage is a failure. This
    is a restart-plus-archive probe: whether the value came from a recent BAL (promotion) or a
    fetch is reported (skim_status.store.bal_promotions), not assumed; BAL -> archive promotion
    itself is covered by deterministic tests
  - config B puts the pool under `storage: all` while its keys come back from the restart
    snapshot as provisional TTL entries, and random pool slots are read on demand right away
    (TTL -> whole-storage promotion); those slots keep being compared long after the TTLs
  - restarts replay a snapshot (--data-dir) while subscriptions fetch fresh (restore race)

Chaos, every 6-12 minutes: SIGKILL + restart, graceful restart, or restart with the *other*
config (key-set change). The archive file and data dir persist across all of them.

Gates (exit status nonzero on any):
  - any WRONG answer
  - subscribed keys over max_subscribed_keys, or RSS over RSS_LIMIT_MB if set
  - availability: outside the recovery window after each restart, subscribed probes answered
    >= 95% and cold probes >= 80%, counting skim timeouts and connection failures as unanswered
    (refusing or timing out on everything fails); every required probe kind sampled at least
    its minimum number of times
  - freshness: in steady state (a disruption starts just before the process is stopped and
    ends when it has recovered, so scheduled downtime never counts), no more than 2% of the
    time with skim's head stale:
    its header timestamp more than 3 slots behind the wall clock, or more than 3 blocks behind
    the reference node's head. Sampled every 2 s by a separate thread, so slow probes can't
    delay it, and by head age rather than arrival gaps, so a stream of steadily arriving but
    old blocks fails. The longest gap between new heads is reported separately
  - recovery: subscriptions ready again within RECOVERY_LIMIT_S (default 180) of each restart

`--selftest` checks the classification rules (reverts and halts are outcomes to compare).
Reorgs are not injected here (live network); they're covered by deterministic store tests.
"""
import json, os, random, signal, subprocess, sys, threading, time, urllib.request

if __name__ == "__main__" and len(sys.argv) > 1 and sys.argv[1] == "--selftest":
    pass  # handled below, after the helpers are defined
else:
    BIN, OUT = sys.argv[1], sys.argv[2]
MINUTES = float(sys.argv[3]) if len(sys.argv) > 3 and sys.argv[1] != "--selftest" else 120
REF = os.environ.get("REF_URL", "")
PORT = 8547
RECOVERY_LIMIT_S = float(os.environ.get("RECOVERY_LIMIT_S", "180"))
SUBSCRIBED_KINDS = ("quote_sub", "slot_sub", "archived", "restart_archive")
# Every run must exercise these, at least this many times (outside recovery windows).
REQUIRED_KINDS = {"quote": 100, "quote_sub": 50, "slot_sub": 50, "slot_cold": 50, "blockhash": 50, "gasprice": 50,
                  "bal_first": 50, "archived": 20, "restart_archive": 10, "promo_slot": 10}
STALE_AFTER_S = 36  # head age (wall clock - header timestamp) beyond 3 slots
STALE_LAG_BLOCKS = 3  # or this many blocks behind the reference head
FRESH_EVERY_S = 2
SUBSCRIBED_MIN, COLD_MIN = 0.95, 0.80
SKIM = f"http://127.0.0.1:{PORT}"
MAX_KEYS = 60
RSS_LIMIT_MB = float(os.environ.get("RSS_LIMIT_MB", "0"))  # 0 = no limit
if sys.argv[1] != "--selftest":
    os.makedirs(OUT, exist_ok=True)

QUOTER = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
POOL = "0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50"
WETH, USDC = "fff9976782d46cc05630d1f6ebab18b2324d6b14", "1c7d4b196cb0c7b01d743fbc6116a902379c7238"
QUOTE_1 = ("0xc6a5026a" + WETH.rjust(64, "0") + USDC.rjust(64, "0") + f"{10**18:x}".rjust(64, "0")
           + f"{3000:x}".rjust(64, "0") + "0" * 64)

LIMITS = {"A": MAX_KEYS, "B": 8000}
CONFIGS = {
    "A": f"""
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
  limits: {{ max_subscribed_keys: {MAX_KEYS} }}
predictors:
  uniswap_v3:
    pools: ["{POOL}"]
    prefetch_budget: 16
""",
    "B": f"""
state:
  subscriptions:
    - name: pool
      address: "{POOL}"
      account: true
      storage: ["0x0", "0x1", "0x2", "0x4"]
      retention: {{ mode: archive }}
    - name: weth
      address: "0x{WETH}"
      account: true
    - name: pool-all
      address: "{POOL}"
      storage: all
      max_slots: 20000
  limits: {{ max_subscribed_keys: 8000 }}
""",
}


def promo_config(cfg, promo):
    """The config plus an archived subscription for `promo` (an account whose balance is, at
    this moment, only in recent BALs)."""
    extra = f"""    - name: promo
      address: "{promo}"
      account: true
      retention: {{ mode: archive }}
"""
    return CONFIGS[cfg].replace("  limits:", extra + "  limits:", 1) if promo else CONFIGS[cfg]


def rpc(url, method, params, timeout=60):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json", "user-agent": "skim-chaos"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    except Exception as e:  # connection refused during restarts, timeouts
        return {"error": {"code": "transport", "message": str(e)}}


def initcode_returning(op_hex):
    """Creation code that executes `op_hex` (leaving one word) and returns it."""
    return "0x" + op_hex + "5f52" + "6020" + "5ff3"  # PUSH0 MSTORE PUSH1 32 PUSH0 RETURN


def blockhash_probe(n):
    return {"data": initcode_returning("67" + f"{n:016x}" + "40")}  # PUSH8 n BLOCKHASH


def random_quote(rnd):
    weth_in = rnd.random() < 0.5
    amt = int(10 ** (14 + 5 * rnd.random())) if weth_in else int(10 ** (3 + 7 * rnd.random()))
    tin, tout = (WETH, USDC) if weth_in else (USDC, WETH)
    return {"to": QUOTER, "data": "0xc6a5026a" + tin.rjust(64, "0") + tout.rjust(64, "0")
            + f"{amt:x}".rjust(64, "0") + f"{3000:x}".rjust(64, "0") + "0" * 64}


class Skim:
    def __init__(self):
        self.proc = None
        self.cfg = "A"
        self.archive = os.path.join(OUT, "chaos.redb")

    def start(self, cfg, promo=None):
        self.cfg = cfg
        path = os.path.join(OUT, f"skim-{cfg}.yaml")
        with open(path, "w") as f:
            f.write(promo_config(cfg, promo))
        log = open(os.path.join(OUT, "skim.log"), "ab")
        self.proc = subprocess.Popen(
            [BIN, "run", "--rpc", f"127.0.0.1:{PORT}", "--cl-port", "9170", "--el-port", "30409", "--window", "32",
             "--config", path, "--archive", self.archive, "--data-dir", os.path.join(OUT, "data")] + os.environ.get("SKIM_EXTRA_ARGS", "").split(),
            stdout=log, stderr=log, env=dict(os.environ, RUST_LOG="info"))

    def stop(self, sig):
        if self.proc:
            self.proc.send_signal(sig)
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


HALT_WORDS = ("out of gas", "execution", "invalid opcode", "stack", "halt", "gas required exceeds")


def outcome(resp, side):
    """Execution outcome of a JSON-RPC response:
    ("ok", result) | ("revert", data) | ("halt",) | ("unavailable", why).
    Reverts and EVM halts are results to compare, not failures; only transport and resource
    errors (or, for skim, its -32002 refusals) are "unavailable"."""
    e = resp.get("error")
    if e is None:
        return ("ok", resp.get("result"))
    code, msg = e.get("code"), str(e.get("message", "")).lower()
    if code == 3:
        return ("revert", e.get("data"))
    if code == -32000 and any(w in msg for w in HALT_WORDS):
        return ("halt",)
    return ("unavailable", f"{side} {code} {msg[:60]}")


def classify(s, r):
    ro, so = outcome(r, "ref"), outcome(s, "skim")
    if ro[0] == "unavailable":
        return "ref_unavailable"
    if so[0] == "unavailable":
        return "refused"
    return "match" if so == ro else "WRONG"


def is_stale(now, head_ts, skim_n, ref_n):
    """skim's head is stale: older than STALE_AFTER_S by its own timestamp, or more than
    STALE_LAG_BLOCKS behind the reference head (None if the reference wasn't sampled)."""
    if head_ts is None:
        return True
    if now - head_ts > STALE_AFTER_S:
        return True
    return ref_n is not None and skim_n is not None and ref_n - skim_n > STALE_LAG_BLOCKS


class Phase:
    """Shared run phase: steady state, or a disruption (from just before the process is stopped
    until it has recovered). Recovery time is measured from the relaunch."""

    def __init__(self):
        self.lock = threading.Lock()
        self.disrupted = True  # the first start is a recovery too
        self.launched_at = time.time()

    def disrupt(self):
        with self.lock:
            self.disrupted, self.launched_at = True, None

    def launched(self):
        with self.lock:
            self.launched_at = time.time()

    def recovered(self):
        with self.lock:
            took = time.time() - self.launched_at
            self.disrupted, self.launched_at = False, None
            return took

    def steady(self):
        with self.lock:
            return not self.disrupted


class Freshness:
    """Steady-state time with a stale head, from samples (scheduled disruptions excluded)."""

    def __init__(self):
        self.stale_s = self.observed_s = 0.0

    def sample(self, dt, steady, now, head_ts, skim_n, ref_n):
        if not steady:
            return
        self.observed_s += dt
        if is_stale(now, head_ts, skim_n, ref_n):
            self.stale_s += dt


def selftest():
    ok = {"result": "0x01"}
    rev = lambda d: {"error": {"code": 3, "message": "execution reverted", "data": d}}
    cases = [
        (ok, rev("0xaa"), "WRONG"),                                   # skim succeeded, ref reverted
        (rev("0xbb"), rev("0xaa"), "WRONG"),                          # both reverted, different data
        (rev("0xaa"), rev("0xaa"), "match"),                          # same revert
        (ok, {"result": "0x02"}, "WRONG"),
        (ok, ok, "match"),
        ({"error": {"code": -32002, "message": "unavailable"}}, ok, "refused"),
        (ok, {"error": {"code": "transport", "message": "timeout"}}, "ref_unavailable"),
        (ok, {"error": {"code": -32000, "message": "header not found"}}, "ref_unavailable"),
        ({"error": {"code": -32000, "message": "execution halted: OutOfGas"}}, {"error": {"code": -32000, "message": "out of gas"}}, "match"),
        (ok, {"error": {"code": -32000, "message": "out of gas"}}, "WRONG"),  # ref halted, skim didn't
    ]
    bad = [(s, r, want, classify(s, r)) for s, r, want in cases if classify(s, r) != want]
    # Freshness by head age, not arrival: a block arriving now but stamped 5 minutes ago is
    # stale, every time, however regularly such blocks keep coming.
    t = 1_000_000.0
    fresh_cases = [
        ((t, t - 6, 100, 100), False),
        ((t, t - 300, 100, 100), True),   # old header, arriving on time
        ((t, t - 6, 100, 125), True),     # recent timestamp claim, but 25 blocks behind the reference
        ((t, t - 6, 100, None), False),   # reference not sampled: age alone decides
        ((t, None, None, 100), True),     # no head at all
    ]
    for args, want in fresh_cases:
        if is_stale(*args) != want:
            bad.append(("is_stale", args, want))
    # Steadily arriving old headers fail the gate end to end.
    share = sum(is_stale(t + 12 * i, t + 12 * i - 300, 100 + i, 125 + i) for i in range(100)) / 100
    if share <= 0.02:
        bad.append(("steady old stream passes", share))
    # Scheduled downtime isn't steady-state staleness; an outage after recovery is.
    f = Freshness()
    for i in range(15):  # stopped on purpose (disruption): no head at all
        f.sample(2, False, t + 2 * i, None, None, 125)
    for i in range(15):  # recovered, fresh
        f.sample(2, True, t + 30 + 2 * i, t + 30 + 2 * i - 4, 125, 125)
    if f.stale_s != 0:
        bad.append(("scheduled downtime counted as stale", f.stale_s))
    for i in range(5):  # unexpected outage in steady state
        f.sample(2, True, t + 60 + 2 * i, None, None, 126)
    if f.stale_s != 10:
        bad.append(("unexpected outage not counted", f.stale_s))
    cases = cases + fresh_cases + [None, None]
    for b in bad:
        print("SELFTEST FAIL", b)
    print(f"selftest: {len(cases) - len(bad)}/{len(cases)} ok")
    sys.exit(1 if bad else 0)


if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
    selftest()


counts = {"match": 0, "refused": 0, "WRONG": 0, "ref_unavailable": 0, "skim_transport": 0}
by_kind = {}
steady = {}  # kind -> [answered, refused] outside recovery windows
recoveries = []  # per restart: seconds until subscriptions were ready again (None = never)
phase = Phase()
promo = None  # (address, block it was subscribed at)
senders_seen = []
last_n = None
max_head_gap_s = 0.0  # longest wall-clock gap between new heads (reported, not gated)
steady_s = 0.0
last_tick = time.time()
promo_from = None  # first block skim reports the restart_archive key covered from
bal_promotions = {"start": None, "end": None}
CHAOS_CYCLE = ["switch-config", "sigkill", "graceful", "switch-config", "graceful", "sigkill"]
chaos_n = 0
wrong_log = open(os.path.join(OUT, "wrong.jsonl"), "a")
status_log = open(os.path.join(OUT, "status.jsonl"), "a")
events = open(os.path.join(OUT, "events.jsonl"), "a")
skim = Skim()
skim.start("A")
phase.launched()
t_end = time.time() + MINUTES * 60


fresh = Freshness()


def freshness():
    """Samples head age and lag every FRESH_EVERY_S, independently of the probe loop."""
    global max_head_gap_s
    prev_t, last_hash, last_new = time.time(), None, time.time()
    ref_n, ref_t = None, 0.0
    while time.time() < t_end:
        time.sleep(FRESH_EVERY_S)
        now = time.time()
        h = rpc(SKIM, "eth_getBlockByNumber", ["latest", False], timeout=3).get("result")
        if now - ref_t >= 6:  # the reference is sampled less often
            r = rpc(REF, "eth_blockNumber", [], timeout=5).get("result")
            ref_n, ref_t = (int(r, 16) if r else None), now
        now = time.time()
        if h and h["hash"] != last_hash:
            last_hash = h["hash"]
            steady = phase.steady()
            max_head_gap_s = max(max_head_gap_s, now - last_new) if steady else max_head_gap_s
            last_new = now
        dt, prev_t = now - prev_t, now
        fresh.sample(dt, phase.steady(), now, int(h["timestamp"], 16) if h else None, int(h["number"], 16) if h else None, ref_n)


threading.Thread(target=freshness, daemon=True).start()
next_chaos = time.time() + random.uniform(6, 12) * 60
last = None
first_block = None
over_limit = 0
rss_max = 0.0
rnd = random.Random(7)

while time.time() < t_end:
    if time.time() >= next_chaos:
        # A fixed cycle (not a random pick), so every run switches configs: config B's whole-
        # storage promotion and the BAL -> archive promotion must happen, not just may.
        kind = CHAOS_CYCLE[chaos_n % len(CHAOS_CYCLE)]
        chaos_n += 1
        cfg = ("B" if skim.cfg == "A" else "A") if kind == "switch-config" else skim.cfg
        if not phase.steady():
            recoveries.append(None)  # the previous restart never recovered
        phase.disrupt()  # before the stop: the downtime is scheduled, not steady state
        skim.stop(signal.SIGKILL if kind == "sigkill" else signal.SIGTERM)
        events.write(json.dumps({"t": time.time(), "event": kind, "config": cfg}) + "\n"); events.flush()
        print(f"chaos: {kind} -> config {cfg}", flush=True)
        time.sleep(rnd.uniform(5, 40))
        # BAL -> archive promotion: archive the balance of an account that just transacted.
        promo = (senders_seen[-1], None) if senders_seen else None
        promo_from = None
        skim.start(cfg, promo[0] if promo else None)
        phase.launched()
        next_chaos = time.time() + rnd.uniform(6, 12) * 60
    head = rpc(SKIM, "eth_getBlockByNumber", ["latest", False], timeout=5).get("result")
    now = time.time()
    if phase.steady():
        steady_s += now - last_tick
    last_tick = now
    if not head or head["hash"] == last:
        time.sleep(1)
        continue
    last = head["hash"]
    n = int(head["number"], 16)
    last_n = n
    first_block = first_block or n
    if promo and promo_from is None:
        cov = rpc(SKIM, "skim_archiveCoverage", [promo[0]], timeout=5).get("result") or []
        bal = [c for c in cov if c["key"].startswith("Balance")]
        if bal:
            promo_from = bal[0]["from"]
    tag = {"blockHash": head["hash"]}
    sub = rpc(SKIM, "skim_subscriptionStatus", [], timeout=5).get("result")
    ready = bool(sub) and all(x.get("ready") for x in sub)
    recovering = not phase.steady()
    if recovering and ready:
        recoveries.append(phase.recovered())
        events.write(json.dumps({"t": time.time(), "event": "recovered", "seconds": recoveries[-1]}) + "\n"); events.flush()
        recovering = False
    blk = rpc(SKIM, "eth_getBlockByNumber", [head["number"], True], timeout=10).get("result") or {}
    senders = sorted({t["from"] for t in blk.get("transactions", [])})
    if senders:
        senders_seen = senders[-20:]
    reqs = [("quote", "eth_call", [random_quote(rnd), tag]) for _ in range(3)]
    reqs += [("quote_sub", "eth_call", [{"to": QUOTER, "data": QUOTE_1}, tag])]
    reqs += [("slot_sub", "eth_getStorageAt", [POOL, hex(s), tag]) for s in (0, 4)]
    reqs += [("slot_cold", "eth_getStorageAt", [POOL, hex(rnd.randrange(5, 12)), tag])]
    reqs += [("blockhash", "eth_call", [blockhash_probe(n - rnd.randrange(1, 257)), tag])]
    reqs += [("gasprice", "eth_call", [{"data": initcode_returning("3a"), "gas": "0x4c4b40", "gasPrice": hex(rnd.randrange(1, 10**10))}, tag])]
    if n - first_block > 40:
        old = rnd.randrange(first_block, n - 35)
        reqs += [("archived", "eth_getStorageAt", [POOL, "0x0", hex(old)])]
    # BAL-first: balances of this block's senders (written by this block, not tracked).
    reqs += [("bal_first", "eth_getBalance", [a, tag]) for a in senders[:3]]
    # BAL -> archive: the promoted account's balance at blocks after its promotion, from disk.
    if promo and promo_from is not None and n - 33 > promo_from:  # older than the window: from disk
        reqs += [("restart_archive", "eth_getBalance", [promo[0], hex(rnd.randrange(promo_from, n - 33))])]
    # TTL -> whole storage (config B): random pool slots, read on demand from the first block.
    if skim.cfg == "B":
        reqs += [("promo_slot", "eth_getStorageAt", [POOL, hex(rnd.randrange(0, 16)), tag])]
    for kind, m, p in reqs:
        s = rpc(SKIM, m, p)
        r = rpc(REF, m, p)
        # A skim timeout or connection failure is an unanswered request, not a skipped one.
        c = "skim_transport" if s.get("error", {}).get("code") == "transport" and r.get("error") is None else classify(s, r)
        counts[c] += 1
        by_kind.setdefault(kind, {}).setdefault(c, 0)
        by_kind[kind][c] += 1
        if not recovering and c in ("match", "refused", "skim_transport"):
            st_k = steady.setdefault(kind, [0, 0])
            st_k[0 if c == "match" else 1] += 1
        if c == "WRONG":
            wrong_log.write(json.dumps({"block": n, "kind": kind, "method": m, "params": p, "skim": s, "ref": r}) + "\n")
            wrong_log.flush()
    st = rpc(SKIM, "skim_status", [], timeout=5).get("result")
    if st:
        bp = st["store"].get("bal_promotions", 0)
        bal_promotions["start"] = bal_promotions["start"] if bal_promotions["start"] is not None else bp
        bal_promotions["end"] = bp
    rss = subprocess.run(["ps", "-o", "rss=", "-p", str(skim.proc.pid)], capture_output=True, text=True).stdout.strip()
    rss_max = max(rss_max, int(rss or 0) / 1024)
    if st and st["store"]["subscribed_keys"] > LIMITS[skim.cfg]:
        over_limit += 1
    status_log.write(json.dumps({"t": time.time(), "block": n, "config": skim.cfg, "rss_mb": int(rss or 0) / 1024,
                                 "status": st, "subs": sub}) + "\n")
    status_log.flush()
    print(f"block {n} [{skim.cfg}] {counts} rss={int(rss or 0)//1024}MB subscribed={st and st['store']['subscribed_keys']}", flush=True)

skim.stop(signal.SIGTERM)
failures = []
if counts["WRONG"]:
    failures.append(f"{counts['WRONG']} wrong answers (see wrong.jsonl)")
if over_limit:
    failures.append(f"subscribed keys over the limit in {over_limit} samples")
if RSS_LIMIT_MB and rss_max > RSS_LIMIT_MB:
    failures.append(f"RSS {rss_max:.0f} MB over the {RSS_LIMIT_MB:.0f} MB limit")
pending_recovery = None
if not phase.steady():
    # Still recovering when the run ended: unknown unless it already exceeded the limit.
    waited = time.time() - (phase.launched_at or time.time())
    if waited > RECOVERY_LIMIT_S:
        recoveries.append(None)
    else:
        pending_recovery = round(waited, 1)
answer_rate = {k: round(a / (a + r), 4) for k, (a, r) in steady.items() if a + r}
for k, rate in answer_rate.items():
    need = SUBSCRIBED_MIN if k in SUBSCRIBED_KINDS else COLD_MIN
    if rate < need:
        failures.append(f"{k}: answered {rate:.1%} in steady state (< {need:.0%})")
for k, need in REQUIRED_KINDS.items():
    got = sum(steady.get(k, [0, 0]))
    if got < need:
        failures.append(f"{k}: {got} steady-state samples (< {need})")
    if not by_kind.get(k, {}).get("match"):
        failures.append(f"{k}: never matched")
stale_share = fresh.stale_s / fresh.observed_s if fresh.observed_s else 1.0
if stale_share > 0.02:
    failures.append(f"head stale (> {STALE_AFTER_S} s old or > {STALE_LAG_BLOCKS} blocks behind the reference) for {stale_share:.1%} of steady-state time")
slow = [r for r in recoveries if r is None or r > RECOVERY_LIMIT_S]
if slow:
    failures.append(f"{len(slow)} restart(s) not ready within {RECOVERY_LIMIT_S:.0f} s: {slow}")
summary = {"minutes": MINUTES, "counts": counts, "by_kind": by_kind, "steady_answer_rate": answer_rate,
           "steady_samples": {k: sum(v) for k, v in steady.items()}, "stale_share": round(stale_share, 4),
           "max_head_gap_s": round(max_head_gap_s, 1),
           "bal_promotions_during_run": (bal_promotions["end"] or 0) - (bal_promotions["start"] or 0),
           "recoveries_s": [None if r is None else round(r, 1) for r in recoveries], "pending_recovery_s": pending_recovery,
           "subscribed_over_limit_samples": over_limit,
           "rss_max_mb": rss_max, "verdict": "FAIL" if failures else "PASS", "failures": failures}
json.dump(summary, open(os.path.join(OUT, "summary.json"), "w"), indent=2)
print(json.dumps(summary, indent=2))
sys.exit(1 if failures else 0)
