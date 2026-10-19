# M0: adaptive SNAP scheduler, snap/1, head advertising (results)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs on 2026-10-15 (KST), from the same home connection as earlier spikes. Raw data is in `data/ab-*.jsonl`, `data/snap1-check.*` and `data/probe-call-5.*`.

## 1. Head advertising (A/B, 15 min each, run concurrently)
`probe-el --advertise-head` sends our real head in the eth Status and in `BlockRangeUpdate` (range `[h, h]`). The control kept the default genesis status.

| | Advertised head | Genesis status |
|---|---|---|
| snap/2 sessions obtained | **6** | 2 |
| Nethermind session lifetimes (s) | 4, 17, 59, 148 (plus 2 still open) | 23, 28 |
| **SNAP answers received** | **123** | 5 |
| eth/71 BALs received | 188 | 183 |

Advertising the head got **about 25× more SNAP service**. BAL throughput was the same, so the difference is in how snap/2 peers accept and keep us. Caveat: both arms ran at the same time from one IP, alongside a local node, so they may have competed for per-IP slots. The effect size makes the direction clear. **Head advertising is now always on** (`ElNet` background head follower).

## 2. snap/1 client (Geth)
skim now also speaks snap/1 as an RLPx satellite protocol (`skim-el/src/snap1.rs`). RLPx negotiates the highest shared version, so each peer lands on exactly one path.

| (8 min, `probe-el`, EIP-2935 self-check) | Geth (snap/1) | Nethermind (snap/2) |
|---|---|---|
| Storage lookups, all correct | **63/63** | 29/29 |
| Storage P50 / P95 | **268 / 397 ms** | 288 / 334 ms |
| SNAP-capable peers at the end | **4** | 2 |

## 3. Scheduler-driven cold calls (`probe-call-5`, 6 iterations, 7 calls)
Every fetch went through `skim-snap::Scheduler`:
- per-peer in-flight cap of 8
- re-queue on disconnect, timeout or unavailable root
- hedging after 1.2 s
- code batching

Hinted runs use the access set learned per call **shape** (target + selector), as a union over earlier blocks *and other inputs*.

| Call | cold k | cold keys | cold P50 | hinted k (by iteration) | hinted keys | **hinted P50** | Infura first / warm P50 |
|---|---|---|---|---|---|---|---|
| WETH `balanceOf` | 2 | 3 | 266 | 1,1,1,1,1 | 3 | **110** | 689 / 218 |
| USDC proxy `balanceOf` | 5 | 6 | 601 | 1,1,1,1,1 | 6 | **306** | 676 / 216 |
| V3 single 0.01 WETH→USDC | 8 | 65 | 1555 | 1,1,1,1,1 | 1094 ¹ | 7519 ¹ | 677 / 223 |
| V3 single 1 WETH→USDC | 8 | 70 | 1987 | **3**,1,1,1,1,1 | 1094 ¹ | 7512 ¹ | 692 / 230 |
| V3 single 100 WETH→USDC | 11 | 1138 | 9568 | **7**,1,1,1,1,1 | 1094 ¹ | 7541 ¹ | 691 / 235 |
| V3 single 10 USDC→WETH | 8 | 85 | 1739 | **2**,1,1,1,1,1 | 1094 ¹ | 7535 ¹ | 762 / 229 |
| V3 multihop 0.01 WETH | 10 | 487 ² | 4940 | 1,1,1,1,1 | 28 | **562** | 675 / 251 |

Correctness: **80/80 outputs byte-match Infura**, with 0 mismatches.

Scheduler totals:
- 35,574 jobs, 35,570 ok, **0 failed**
- 25 disconnects, all re-queued and completed
- 2 hedges, 2 late answers ignored, 0 root-unavailable

Peer load followed EWMA latency:

| Peer | Client | EWMA | Requests served |
|---|---|---|---|
| `0xc1422d3d` | Geth v1.17.8 | **71 ms** | 18,934 |
| `0x5df776bf` | Nethermind | 288 ms | 7,162 |
| `0xe23b44ce` | Geth v1.17.6 | 309 ms | 6,082 |
| `0xe9556795` | Nethermind | 284 ms | 511 |

None of the four serving peers disconnected during the run.

¹ The union over inputs grew to 1,094 keys, mostly tick words from the 100-WETH quote. The over-fetch made every small quote slower than cold.
² Discovery keys inflated by placeholder divergence. The real access set is 28 keys.

## Findings
1. **The fetch wave is reliable on public peers.** Nothing was lost across 25 disconnects. The scheduler, not the session, owns each request.
2. **With a nearby peer, k=1 beats Infura.** WETH hinted P50 is 110 ms against 218 ms for Infura warm. USDC proxy is 306 ms against 216 ms. Placement by EWMA finds the fast peer by itself.
3. **Learned sets generalize partially.** After learning at 0.01 WETH:
   - 1 WETH needs k=3
   - reversed direction needs k=2
   - 100 WETH, which crosses many ticks, needs k=7

   After one run per input, everything is k=1.
4. **A naive union over-fetches.** One large-input run poisons the shape's learned set (1,094 keys), and a 1,094-key wave costs about 7.5 s. Learned sets need:
   - **frequency weighting:** keep keys seen in at least x% of recent runs
   - **a size cap** that limits the first wave
   - possibly **input bucketing**, e.g. by amount magnitude
5. **Placeholder divergence** (487 discovery keys vs 28 real for multihop) remains the main source of cold over-fetch (§7.2 divergence control).

## Status of the go/no-go risk
The biggest uncertainty was whether a fetch wave can complete reliably on public peers. In this run it did: 0 failed jobs and 4 stable SNAP peers, after head advertising and snap/1. This is one ~17-minute run from one location. The next check is a multi-hour soak.
