# M0 Spike C: cold-call economics vs Infura (first results)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Run: 2026-10-14, from the same home connection as Spike B (Korea). Command: `skim probe-call`. skim and Infura are compared on the same calldata at the same block (`eth_call` with EIP-1898 `{blockHash}`). Raw data is in `data/probe-call-*.json|log`.

## Method
For each call at a fresh head H:
- **skim cold:** start from an empty cache, then loop:
  1. a recording revm pass (Amsterdam rules, `eth_call` semantics, no fee charge, so the per-block coinbase is never loaded)
  2. a parallel SNAP fetch of every miss at `stateRoot(H)`

  This is the naive version: an account that reveals a code hash chains `GetByteCodes` inside the same round, so one round can cost 2 RTTs.
- **skim hinted:** an empty state cache, but bytecode kept (content-addressed, §5.1). The access set recorded for the same call at an earlier block is prefetched as round 1. This is the §7.2 "previous access set" source.
- **skim warm:** local re-execution once every key is known.
- **Infura:** the first call on a fresh HTTPS client (TLS included), then warm keep-alive repeats.
- **Correctness:** skim's output must byte-match Infura's.

## Results

From `probe-call-3` and `probe-call-4`. All values in ms unless marked. P50/P95.

| Call | k cold | keys | skim cold | **skim hinted (k=1)** | skim warm exec | Infura first call | Infura warm |
|---|---|---|---|---|---|---|---|
| WETH `balanceOf` | 2 | 3 | 902 / 1045 | **309 / 381** | 18 µs | 673 / 810 | 219 / 597 |
| USDC (proxy) `balanceOf` | 5 | 6 | 1934 / 2386 | **352 / 639** | 25 µs | 657 / 701 | 207 / 299 |
| Uniswap V3 quote, single pool | 8 | 65 | 5004–5453 ³ | **785–937** | 350 µs | 676 | 211 |
| Uniswap V3 quote, multihop | 9 | 807 ¹ | failed ² | failed ² | — | 669 | 294 |

**Outputs: every completed skim run matched Infura byte for byte** (20+ matches, 0 mismatches after the fix below).

¹ Placeholder divergence: zero placeholders for tick-bitmap words make the discovery pass walk the wrong path and request far more keys than the real execution touches.
² The serving peer evicted us mid-run (see below).
³ With `--max-inflight 8`. Unlimited fan-out (65 parallel requests) failed outright.

## Findings
1. **One SNAP round ≈ one Infura warm call** (≈250–300 ms vs ≈210 ms from here). So:
   - warm (k=0): skim wins by four orders of magnitude (µs vs ms)
   - k=1: comparable to Infura warm, and **beats Infura's first call** (≈650 ms, which includes TLS)
   - k≥2: Infura wins
2. **A learned access set collapses k to 1** for every call that completed: WETH 2→1, USDC proxy 5→1, V3 quote 8→1. This is the strongest result: a repeated call shape becomes a single round. The remaining cost is the round's width (65 keys for the V3 quote, limited by per-peer concurrency), not its depth.
3. **Naive cold k is large.** Proxies (5) and AMM quotes (8–9) are dominated by data-dependent chains. Without prediction, cold calls take 1–5 s.
4. **Fan-out has to be spread across peers.** Firing dozens of concurrent requests at one peer gets them dropped. Width needs more peers, not more concurrency per peer.
5. **Public snap/2 peers evict us.** Nethermind peers accept the session, then disconnect us with `TooManyPeers` after 22–51 s (7 sessions in 8 minutes), and in-flight requests are lost (1,048 dropped). A pinned pool of *public* peers isn't stable as-is. Options:
   - look like a useful peer: advertise the real head and send `BlockRangeUpdate`
   - add snap/1 to reach Geth peers
   - run a dedicated snap peer
6. **Bugs found and fixed during the spike:**
   - Request errors had been coerced to zero values. That caused a mismatch and is now an explicit failure.
   - Fee charging loaded the coinbase, adding one useless round per call. It's now disabled.

## Implications for the design
- §7.2 prediction is mandatory, not an optimization. The "previous access set for the same call shape" source alone takes partially cold calls to Infura-class latency.
- Discovery passes need divergence control. Cap keys per round, or fetch in waves that stop early, so placeholder walks don't over-fetch (the multihop case).
- The resolver must spread a round's keys across several pinned peers, with per-peer in-flight limits (§6.3.1).
- Peer retention is the gating risk for M0's go/no-go. Next experiments, in order:
  1. advertise the real head and see whether eviction stops
  2. snap/1 client support (the Geth majority)
  3. a dedicated snap/2 peer as the control case
