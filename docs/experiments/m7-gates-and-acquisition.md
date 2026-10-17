# M7: regression gates, a stricter soak, and peer acquisition

Runs: 2026-10-17 (KST), Sepolia.

Order of work, as agreed after the fourth review:
1. close the known correctness findings with deterministic regression tests, in CI
2. a soak that exercises the dangerous transitions and gates on availability too
3. first-use peer acquisition, with consensus catch-up diagnostics alongside
4. diagnostics, alerts and release packaging
5. memory and throughput

## 1. Correctness findings and CI
The fourth review's findings (storage-mode transitions, `safe`/`finalized` resolution) are fixed with a regression test each; see [M6](m6-correctness-and-bounds.md#fourth-review-storage-mode-transitions). The store tests fail on the previous code.

**CI** (`.github/workflows/ci.yml`) runs on every push and pull request:
- **tests:** the workspace test suite, which includes every review regression
- **fuzz:** a replay of past fuzz crashes

Clippy runs in report-only mode, because older warnings remain. Every push since has passed.

## 2. Soak v2: transitions and availability
`docs/experiments/tools/chaos_soak.py` compares skim with a reference node every block, as before. It now also creates the transitions the reviews found dangerous:

| Transition | How the soak creates it | Probe |
|---|---|---|
| BAL → archive | at each restart, the config gains an archived subscription for an account whose balance, at that moment, exists only in recent BALs | `promo_archived`: that balance at blocks older than the in-memory window (from disk) |
| TTL → whole storage | config B puts the pool under `storage: all` while the restart snapshot hands its keys back as provisional TTL entries, and random pool slots are read on demand from the first block | `promo_slot`: those slots, every block, long after the TTLs |
| BAL-first reads | – | `bal_first`: balances of the current block's senders |
| Fresh fetch vs restore | every restart (the soak keeps a `--data-dir`) | all probes |

Reorgs can't be triggered on a live network; they're covered by the deterministic store tests.

**Gates.** A run fails on any of:
- a wrong answer
- subscribed keys over the limit, or RSS over its limit
- a steady-state answer rate below 95% for subscribed probes or 80% for cold ones (so a client that refuses everything fails)
- a probe kind that never matched
- subscriptions not ready again within 180 s of any restart

**Smoke run (15 min): PASS.**
- **Answers:** 665 matches, 3 refusals, 0 wrong. Every steady-state answer rate was 100%.
- **Recovery:** 142 s (cold start, no snapshot) and 91 s.

That run found two problems, fixed below: a 3-block restart gap downloaded 43 MB, and the first post-restart subscription pass waited for SNAP peers.

**Full run (2 h):** see [Results](#results).

## 3. Peer acquisition
**Measured** over 7 Sepolia startups, from skim's session log:

| Startup | EL sessions | Crawlers | reth (no SNAP) | SNAP-capable | First SNAP peer | SNAP peers that dropped us ("too many peers") |
|---|---|---|---|---|---|---|
| cold | 28 | 16 | 6 | 6 | 61 s | 3 |
| cold | 59 | 36 | 13 | 10 | 45 s | 5 |
| cold | 48 | 32 | 10 | 6 | 47 s | 3 |
| saved peers | 14 | 7 | 3 | 4 | 15 s | 3 |
| saved peers | 16 | 11 | 4 | 1 | 152 s | 0 |
| saved peers | 60 | 30 | 20 | 10 | 6 s | 3 |
| saved peers | 17 | 8 | 6 | 3 | 6 s | 1 |

What the measurements say:
- About half the sessions are crawlers, and reth peers don't offer SNAP.
- About half the SNAP-capable peers end the session within a minute because they're full.
- Saved peers usually get a state peer in seconds, but not always.

So more dialing isn't the lever. Knowing which peers stay is.

**Changes:**
- **`PeerBook`:** records for each peer its client, SNAP support, longest session, and how often it dropped us for being full.
- **Ranked saved list:** peers that held a SNAP session rank first. Crawlers and peers that only ever dropped us are left off the list, even if an older list had them.
- **Metrics and status:** sessions by client and SNAP support, session ends by reason, time to the first SNAP peer, and SNAP peers seen and held (≥ 60 s). Shown in `skim_status.peer_acquisition` and in a Grafana row.

**After a restart**, subscription passes now log their duration. Measured: the first pass started with the first block and no SNAP peer, and took 40 s: 25 s waiting for the first SNAP peer, then ~15 s of fetching. Restart readiness is bounded by state-peer acquisition, not by the subscription logic.

### Consensus catch-up
Every restart's consensus catch-up had timed out. Instrumented first (see [M6](m6-correctness-and-bounds.md#restart-catch-up-via-consensus-peers-measured-then-fixed)):
- **Cause:** peers served every request, but sequential 2–6 s requests used up the 60 s budget.
- **Parallelism:** 8 requests in flight → a 655-block gap in 8.4–9.0 s.
- **Sizing:** the walk now starts only as many windows as the gap needs. A 3-block gap had downloaded 43 MB (262 envelopes). A 48-block gap now takes 61 envelopes, 8.9 MB, 2.6 s.

## 4. Diagnostics, alerts, packaging
- **Alerts:** four Grafana rules, provisioned with the stack: no SNAP peers, head behind, subscription not ready, refusals. They were checked to load and evaluate, and "head behind" fired when the node stopped reporting.
- **Container image:** a `Dockerfile` with a slim runtime that runs as a non-root user.
- **Release workflow:** a version tag builds Linux x86-64 and arm64 and macOS arm64 binaries and publishes the image to GHCR. A manual dry run built all three binaries and the image.

## 5. Memory
Each block in the recent-BAL index holds, on Sepolia:

| Part | Entries / bytes per block | Approx. memory per block |
|---|---|---|
| writes | 727 | ~70 KB |
| read-only slots | 776 | ~43 KB |
| touched addresses | 366 | ~7 KB |
| deployed bytecode | 4.5 KB | 4.5 KB |

Only the writes are read again after a block is applied, so the index now keeps only those: about 44% fewer entries, ~14 MB less with the default 256-block window. This estimate comes from entry counts and in-memory sizes, not a measured RSS difference. `skim_status.store.recent_index` shows the breakdown.

## Results
(full soak: filled in when it completes)
