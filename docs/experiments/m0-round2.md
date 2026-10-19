# M0 round 2: learned-set hygiene, peer-count sweep, local-peer control, CL gossip timing

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs on 2026-10-15 (KST), same home connection as before. A local Nethermind 2.1.0 + Lighthouse v8.3.0-rc.0 node shared the same public IP for most of these runs, so it competed for per-IP peer slots. Raw data is in `data/probe-call-6.*`, `data/probe-wave-1.*`, `data/probe-call-local.*`, `data/cl-sse-lag.jsonl` and `data/node/lighthouse.log`.

## 1. Learned-set hygiene (`probe-call-6`)
Policy (then `skim-cli/src/learned.rs`; the current implementation is in [`skim-sim/src/learned.rs`](../../crates/skim-sim/src/learned.rs)):
- **exact:** the last access set for identical calldata
- **core:** for a new input of a known shape, keys seen in ≥ 50% of the shape's last 8 runs, capped at 256

| Call | exact k | core k | hinted keys | hinted P50 (ms) | Before (union) |
|---|---|---|---|---|---|
| WETH `balanceOf` | 1 | – | 3 | 270 | 110–309 |
| USDC proxy | 1 | – | 6 | 292 | 306 |
| V3 single 0.01 | 1 | – | 18 | **583** | 7,519 |
| V3 single 1 WETH | 1 | 2 | 23 | 1,017 | 7,512 |
| V3 single 10 USDC→WETH | 1 | 2 | 15 | 325 | 7,535 |
| V3 single 100 WETH | 1 | 7 | 1,092 ¹ | 10,604 | 7,541 |
| V3 multihop | 1 | – | 28 | 1,413 | 562 |

Outputs: 79/79 match Infura.

¹ The real access set of this input is 1,092 keys, so the wave width is inherent to the call, not a prediction artifact.

Union poisoning is gone. New inputs of a known shape start from a small core set and need one extra round (k=2), except heavy tick-crossing inputs.

Bug found and fixed: job deadlines counted time spent queueing for peer capacity, so large waves failed with `deadline exceeded`. The deadline now runs from the first send (test: `queueing_for_capacity_is_not_a_deadline`).

## 2. Peer-count sweep (`probe-wave-1`, wave of 1,092 keys, in-flight cap 8 per peer)

| Live peers | max_peers | Wall | Keys/s | Failed |
|---|---|---|---|---|
| 1 | any | 25.7–26.3 s | 41–42 | 0 |
| 3 | all | 16.1 s | 67 | 0 |
| 2 | 1 | 25.8 s | 42 | 0 |
| 2 | 2 / all | **6.4 s** | **169** | 0 |

Throughput tracks **Σ (in-flight_i / RTT_i)**. In the last iterations, a second peer with ~47 ms latency made the wave 4× faster, not 2×. Fast peers matter more than peer count. To fetch a ~1,000-key wave in about 1 s you need either several fast peers or a higher in-flight cap on fast peers. That points to adaptive (AIMD) per-peer concurrency.

Caveat: live SNAP peers were only 1–3 during the sweep, so the 6- and 10-peer points couldn't be measured. Also, `max_peers` selects by EWMA before EWMA is measured, so `max_peers=1` may have picked the slow peer.

## 3. Control: local snap/2 peer only (`probe-call-local`)
Same calls, with skim talking only to the local Nethermind node over loopback (`--trusted … --trusted-only`).

| Call | Cold (ms) | Hinted (ms) |
|---|---|---|
| WETH | 1.3 | 0.6 |
| USDC proxy | 2.7 | 0.8 |
| V3 single 0.01 (k=8) | **8.8** | 1.8 |
| V3 single 100 WETH (1,092 keys) | 102 | 104 |
| V3 multihop (k=10) | 42 | 2.6 |

Outputs 52/52 match, with 0 failures and 0 hedges. **Serving cost is 0.36 ms per lookup.**

So almost all cold-call latency on public peers is network RTT, not serving time. With a dedicated snap peer near skim (same host or datacenter), even k=8 cold calls take under 10 ms, and cold fetch becomes usable on the live path. On public peers, learned sets that reach k=1 remain mandatory.

## 4. CL payload gossip timing (Spike A, cheap version)
The local Lighthouse logs `New envelope received` when a `SignedExecutionPayloadEnvelope` arrives on the `execution_payload` topic. Envelopes carry the full BAL.

| Path to "BAL in hand" | After slot start |
|---|---|
| **CL `execution_payload` gossip** (n = 1,348 slots) | **P50 764 ms**, P95 2,684 ms (min 169, max 5,688) |
| Current EL path: header poll (~1.3 s) + eth/71 BAL (~0.37 s) | ~1.67 s P50 |
| Public EL nodes serving the header (proxy for import done) | ~1.3 s P50 |

The gossip path is about 0.9 s ahead of skim's EL path, and about 0.3–0.5 s ahead of a full node finishing import. This is the first quantitative support for the "BAL from gossip beats a node" hypothesis.

Caveats:
- This measures arrival at Lighthouse, not at a skim-built CL listener.
- It's one vantage point.
- The beacon-API SSE `execution_payload` event fires only after full import (~13 s here), so it is **not** a gossip-timing signal.
- This Lighthouse rc hands envelopes to its EL about 12 s after the slot. Local Nethermind logged `Received New Block` at +12 s. That is a property of this rc, not of the network.

Nethermind executes a 79 MGas block in about 18.5 ms. Block execution is not where a full node loses time; receipt and import timing is.

## Updated picture
| Question | Answer so far |
|---|---|
| Can BALs keep state current? | Yes. 0 mismatches across all runs |
| Can learned sets give k=1? | Yes for repeated inputs; k=2 for new inputs of a known shape with core sets |
| Can fetch waves complete on public peers? | Yes. 0 lost jobs across disconnects, but throughput depends on finding fast peers (Σ in-flight/RTT) |
| Is serving time a bottleneck? | No. 0.36 ms per lookup at the server; RTT dominates |
| Is CL gossip worth building? | Likely. About 0.9 s earlier BAL than the EL path at P50 |
