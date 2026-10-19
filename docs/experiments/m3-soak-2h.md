# Instrumented soak (2 h, randomized workload)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Run on 2026-10-15 (KST) with the configuration of run G: predictor, adaptive window, prefetch budget 256, 8 random V3 quotes per block across 3 fee tiers. It covered 587 blocks (#11918222–11918808) on public peers; Infura was compared every 4th block. Raw data is in `data/soak24/` (health every 10 blocks, slow-call records, per-process samples).

| | Result |
|---|---|
| Local execution | **8,685 / 8,685 calls succeeded** |
| Oracle (Infura at the same block hash) | **2,160 checked, 0 disagree** |
| Call latency | P99 748 ms in the first minutes (cold), then **13 ms** for the rest of the run |
| Calls over 1 s | 1, at warm-up; cause: queueing for peer capacity (974 ms queue, 389 ms service) |
| All calls done per block, after slot start | P50 830 / P95 2,669 / P99 3,293 ms (set by envelope arrival) |
| SNAP peers | 3–32 (median 22); scheduler: 0 failed, 8 disconnects absorbed; 0 proof rejections |
| Background prefetch | 1,440 keys, 460 read (32%) in the first ~30 blocks; then none was needed, because BAL writes keep the keys current |
| Network | 1.30 GB in / 0.20 GB out over 2 h (~650 MB/h in, mostly CL gossip) |

## Growth (the finding)
| | Start | +1 h | +2 h |
|---|---|---|---|
| RSS | 30 MB | ~140 MB | **175 MB** (still rising) |
| Store keys / versions | 4,175 / 4,235 | 4,453 / 5,211 | 4,778 / 5,690 |
| Code entries | 52 | 587 | **1,155** |
| Learned exact entries / keys | 79 / 5.7k | 2,399 / 126k | **4,096 (cap) / 201k** |

1. **Unbounded code cache (bug, fixed).** Every contract deployed on Sepolia shows up in some block's BAL, and all of that code was cached. Code is now kept only for accounts whose `CodeHash` is tracked (test: `code_is_cached_only_for_tracked_accounts`).
2. **Learned set at its entry cap holds ~200k keys.** An entry bound alone doesn't bound memory. Next: generalized patterns (pool + direction + amount bucket + tick range) and key and byte bounds (PROJECT.md §7.2).
3. **The store's key count grows slowly** (+600 in 2 h). Keys learned by this `follow` harness are kept as subscribed. Under the M3 model, `skim run` keeps unsubscribed keys only for a TTL.

The 2-hour run was too short to see RSS plateau. The learned cap was reached only at the end.
