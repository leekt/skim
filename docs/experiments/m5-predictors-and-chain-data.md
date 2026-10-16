# M5: generalized learned sets, the V3 predictor in `skim run`, transactions, receipts and logs

Runs: 2026-10-15, Sepolia, one home connection, public peers.

## 1. Learned sets: a pattern tier, with bounds
`skim_sim::learned` now has three tiers, most specific first:

| Tier | Key | Prediction |
|---|---|---|
| Exact | `to` + full calldata | the last access set for that input |
| **Pattern** (new) | `to` + selector + per-argument class | union of the pattern's last 4 runs |
| Shape | `to` + selector | keys seen in ≥ 50% of the last 8 runs (frequency core) |

**Argument classes:**
- *Exact:* addresses, small integers (< 2²⁴: fee tiers, directions, tick spacings, ABI offsets), and small negative integers.
- *Magnitude bucket:* everything else, i.e. amounts and price limits. One bucket spans a factor of 4.

So `quote(WETH→USDC, 0.3%, 1.2 ETH)` and `quote(WETH→USDC, 0.3%, 1.7 ETH)` share a pattern. `USDC→WETH`, another fee tier, or `100 ETH` do not.

**Bounds:**
- Least-recently-used limits per tier: 4,096 exact entries, 1,024 patterns, 256 shapes.
- A global budget of 500,000 keys across tiers, evicting exact entries first, then patterns, then shapes.
- `sizes()` reports keys, estimated bytes and evictions in the `follow` health records.
- Tests: amount generalization without crossing pools or directions, the global budget, LRU order.

### A/B: patterns on vs off (`skim run`, on-demand TTL 4)
Setup: the same random V3 quotes (8 per block, log-uniform amounts, 3 fee tiers, both directions) were sent to two instances for 50 blocks. Tool: `docs/experiments/tools/learned_ab.py`.

| | Patterns on | Patterns off |
|---|---|---|
| Calls with miss rounds after running | **25** | 40 |
| Keys fetched in miss rounds | **6,507** | 9,023 |
| Calls that ran a predicted fetch first | 31 | 0 |
| Latency P50 / P99 | 1.6 / 4,531 ms | 1.6 / 4,391 ms |

**Verdict: neutral on latency.** Patterns move work from miss rounds into one predicted wave, but the tail is set by large swaps walking ticks, which no learned set can cover for a new size. In `follow` (all fetched keys stay resident) the two arms were identical: 0 predicted waves, 25 miss events each. Patterns are kept because they are bounded and cheap, and they help repeated similar inputs with an eviction TTL. The protocol predictor is what fixes the tail (next section).

## 2. The Uniswap V3 predictor in `skim run`
Until now the input-aware V3 predictor existed only in the `follow` harness. It is now configured in `skim.yaml`:

```yaml
predictors:
  uniswap_v3:
    pools: ["0x3289…EfF1", "0x6Ce0…9b50", "0x6418…C47"]
    words: 2              # bitmap words kept warm on each side (grows with observed reach)
    prefetch_budget: 256  # keys per pool per block, low priority
```

Pools are registered through the simulator itself (`tickSpacing`, `fee`, `token0`, `token1`). Each block, a budgeted low-priority prefetch keeps each pool's tick window warm.

**A/B (same driver and workload, 50 blocks, 400 calls each):**

| | **V3 predictor** | Learned sets only |
|---|---|---|
| Calls that needed the network | **19** | 51 |
| Fetch waves per such call | 1 (16), 2 (1), 3 (2) | 1 (27), 2 (10), 3 (6), 4 (5), 5 (2), 8 (1) |
| Calls over 100 ms | **20** | 51 |
| Latency P50 / P90 / P99 | 2.0 / **5.5** / 4,521 ms | 1.4 / 300 / 2,706 ms |
| Errors | 1 | 0 |

**Known issue:** the **first call of the run** (cold store) predicted the full reach of a large swap. The wave failed after **75 s** and returned an error. The remaining slow calls are wide predicted waves (382–792 keys, 2–7 s). Next: cap a predicted wave and fetch the nearest words first, and promote the observed reach into the background window sooner.

## 3. Transactions, receipts and logs
New JSON-RPC methods:
- `eth_sendRawTransaction`
- `eth_getTransactionByHash` and `eth_getTransactionReceipt`
- `eth_getLogs`
- `eth_getTransactionCount(addr, "pending")`
- block transaction hashes in `eth_getBlockBy*`

How each piece of data is obtained:
- **Send:** the transaction is decoded and its signer recovered. skim rejects it on the wrong chain id, `nonce too low`, or insufficient funds, all checked against verified head state. It is then gossiped to every connected EL peer and re-broadcast every 12 s until a payload includes it (up to 30 min). Blob transactions are rejected because there is no sidecar gossip.
- **Inclusion:** every verified payload's transactions are hashed and indexed (last 2,048 blocks). This needs no extra network traffic.
- **Receipts:** `GetReceipts` (eth/70 format) for that one block, accepted only if the receipts rebuild the header's `receipts_root`. Older bodies (`GetBlockBodies`) must rebuild `transactions_root`. Headers older than the ones skim followed are fetched backwards and accepted only if they link by hash.
- **Logs:** a block is a candidate only if its `logsBloom` matches **and**, for blocks whose BAL skim saw, a filtered address is in the BAL. A contract that emits a log is always in its block's access list. Only candidates' receipts are fetched. The range is capped by `--max-logs-range` (default 2,048).

**Correctness vs Infura** (`docs/experiments/tools/chaindata_check.py`): every comparison matched exactly.

| Check | Result |
|---|---|
| `eth_getTransactionReceipt` + `eth_getTransactionByHash`, 32 txs of the latest blocks | **32 / 32 identical** |
| `getLogs` WETH `Transfer`, 16 / 1,024 blocks | 1 / 166 logs, **identical** |
| `getLogs` `Swap` topic, any address, 16 / 1,024 blocks | 18 / 1,025 logs, **identical** |
| `getLogs` two addresses, 16 / 1,024 blocks | 3 / 269 logs, **identical** |
| `getLogs` by `blockHash` | 175 and 249 logs, **identical** |

**Cost:**

| Query | Candidates (of blocks scanned) | Time |
|---|---|---|
| Address filter, followed blocks (BAL known) | 1 of 16 | ~0 ms (cached receipts) |
| WETH `Transfer`, 1,024 blocks (mostly from before skim started: bloom only) | 180 | 25.7 s |
| Two addresses, 1,024 blocks | 183 | 13.1 s |
| `Swap` topic with no address, 1,024 blocks (bloom saturated) | 881 | 116 s |

Wide historical scans are bounded by receipt downloads from the 1–2 EL peers connected at the time. Narrow filters over followed blocks are nearly free. **End-to-end send** (2026-10-16, `examples/send-transaction/send.mjs`, unmodified viem, a 0-value self-transfer from a test key):
1. skim gave viem the nonce, gas and fees, then broadcast the signed transaction to 3 EL peers.
2. A gossiped payload included it 3.2 s later (block 11,924,541, index 147). viem's `waitForTransactionReceipt` returned success 4.3 s after sending.
3. Compared with Infura afterwards:
   - `eth_getTransactionReceipt`: identical on all 14 shared fields
   - `eth_getTransactionByHash`: identical on all 20 shared fields
   - `eth_getTransactionCount` (latest and pending) and `eth_getBalance`: identical

## 4. Warm-path latency (after the review fixes)
Three things ran on every call, even when every key was already local:
- **Prediction:** the predictor ran before every call, including the V3 tick-range computation.
- **Learning:** every call's access set was recorded.
- **Bytecode analysis:** the bytecode of every contract touched was re-analyzed (`Bytecode::new_raw` jump-table analysis), and the header re-hashed.

**Now:**
- the call runs first
- prediction happens only after a run misses, and its keys go out in the same wave as the actual misses
- only runs that fetched something are learned
- analyzed bytecode is cached by code hash
- header hashes are computed once

Predicted waves are also capped at 192 keys, nearest tick words first, so a huge prediction can't stall a call.

`docs/experiments/tools/latency_bench.py`, same config (`examples/uniswap-v3/skim.yaml`), same machine, sequential calls at `latest` after warm-up (500 per kind):

| | Before | After |
|---|---|---|
| Subscribed quote P50 / P90 / P99 | 0.756 / 0.842 / 1.076 ms | **0.248 / 0.327 / 0.558 ms** |
| Quotes for 10 amounts P50 / P99 | 0.735 / 1.145 ms | **0.216 / 0.447 ms** |
| `balanceOf` P50 | 0.140 ms | 0.123 ms |
| `eth_getStorageAt` P50 (no EVM; ~HTTP floor) | 0.109 ms | 0.114 ms |
| Throughput, 1 / 10 / 50 in flight | 1,260 / 2,675 / 2,798 calls/s | **4,144 / 8,377 / 8,469 calls/s** |

A profile under load now shows the server's time is almost all in the EVM interpreter itself.

## 5. Throughput under concurrency
`skim bench-node` (Rust client, the harness used for the full-node comparison), Uniswap V3 config, 10-core machine. Calls per second at 1 / 10 / 100 in flight:

| | 1 | 10 | 100 |
|---|---|---|---|
| Measured next to a running Nethermind (it uses ~8.5 cores by itself) | 1.5k | 8.7k | 8.9k |
| skim alone, before | 1.5k | 10.7k | 11.4k |
| skim alone, faster hashing for state maps + TTL bookkeeping under a read lock | 1.7k | 13.3k | **13.8k** |
| skim alone, repeated calls with the per-block result cache | 17.0k | 65.9k | **91.0k** |
| Nethermind (measured with skim idle; its own background load ~8.5 cores) | 1.7k | 14.2k | 16.8k |

**Where the time goes:**
- Under load, 79% of skim's CPU is the revm interpreter itself.
- skim's own overhead was ~8% before these changes.
- At 100 in flight, skim saturates ~7.7 of 10 cores.

The earlier 8.5k figure was partly CPU starvation: a full node was running on the same machine.

**Result cache:** state at a block hash never changes, so `eth_call` with an identical request at the same block hash has the same outcome. skim caches results and reverts by (block hash, request), never errors such as `-32002`. Apps that poll the same quotes every block get the cached path. `--no-call-cache` turns it off.

**Native precompile backends:** revm is now built with `secp256k1`, `blst` and `c-kzg` (its defaults; skim had them off). This didn't change this workload's throughput.
