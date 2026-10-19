# M0 soak: 250 blocks of steady-state hinted calls on public peers

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Run on 2026-10-15 (KST), `skim probe-call --soak --iterations 250 --infura-repeats 0 --min-snap-peers 2`. It ran for about 70 minutes, using public Sepolia peers only. The local node was running but never joined the pool. Raw data is in `data/soak-1.*`.

For each new head, the 7 calls run **hinted** from a fresh state cache:
- bytecode is kept, since it's content-addressed
- the access set comes from the learned-set policy (exact input)

Each result is compared with one Infura `eth_call` on a fresh HTTPS client, at the same block hash.

| Call | Runs | skim hinted P50 / P95 (ms) | Infura first-call P50 / P95 (ms) | Match / mismatch |
|---|---|---|---|---|
| erc20_balanceOf (WETH) | 249 | **12** / 151 | 639 / 749 | 250 / 0 |
| proxied_balanceOf (USDC) | 249 | **54** / 223 | 634 / 759 | 250 / 0 |
| v3_single 0.01 WETH->USDC | 249 | **314** / 588 | 628 / 731 | 250 / 0 |
| v3_single 1 WETH->USDC | 250 | **318** / 643 | 629 / 712 | 251 / 0 |
| v3_single 100 WETH->USDC | 250 | **1493** / 2777 | 628 / 738 | 251 / 0 |
| v3_single 10 USDC->WETH | 250 | **288** / 328 | 619 / 727 | 251 / 0 |
| v3_multihop 0.01 WETH | 249 | **319** / 608 | 638 / 734 | 250 / 0 |

- **Rounds:** every one of the 1,746 hinted runs completed in **k=1**.
- **Correctness:** 1,753 / 1,753 outputs match Infura, with 0 errors.

## Scheduler over the hour
| | |
|---|---|
| Jobs | 298,084 |
| **Failed** | **0** |
| Peer disconnects (in-flight requests lost) | 126 |
| Re-queued and completed | 788 |
| Request errors (timeouts etc.) | 18 |
| Hedges | 67 |
| Late duplicate answers ignored | 46 |

Pool: 10 serving peers at the end, with SNAP-capable peers ranging from 2 to 14 over the run.

The top peer was a public **Geth v1.17.7** node that stayed connected for the whole run and served 37,329 requests (EWMA 90 ms). It was likely geographically close, which explains the 12 ms WETH P50.

Session closes (excluding crawlers): Nethermind `TooManyPeers` × 22, `UselessPeer` × 4 (Geth 2, reth 1, Besu 1). None of them caused a failed job.

## Conclusions
1. **The adaptive scheduler meets the "stable fetch wave on public peers" bar** for this hour: 298k jobs, 0 failed, 126 disconnects absorbed.
2. **Steady-state hinted latency beats Infura's first call for every call shape except the 1,092-key quote.** It's competitive with Infura warm (~210–230 ms measured earlier) when a nearby peer exists:
   - balanceOf: 12–54 ms
   - V3 quotes: ~290–320 ms
3. **The wide wave (1,092 keys) is the remaining cost**, at 1.5 s P50. That is the case for AIMD and more fast peers.
4. **Caveats:**
   - one location, one hour
   - a lucky nearby Geth peer
   - the SNAP pool varied from 2 to 14 peers

   Repeat the soak from a different region before generalizing.

## AIMD vs fixed per-peer concurrency (`probe-wave`, 5 blocks, 4 live SNAP peers)
Same learned wave (1,092 keys, V3 100-WETH quote) at each new head:

| Peers used | Mode | Wall P50 / P95 (ms) | Keys/s (P50) | Failed |
|---|---|---|---|---|
| all (4) | aimd | 2313 / 2319 | 472 | 0 |
| all (4) | fixed | 5165 / 5427 | 211 | 0 |
| 2 | aimd | 2647 / 2847 | 413 | 0 |
| 2 | fixed | 6814 / 6838 | 160 | 0 |

**AIMD fetches the same wave about 2.3–2.6× faster than a fixed cap of 8, with 0 failures.** Fast peers grow their in-flight limit, and slow peers stay near their starting cap. AIMD is now the scheduler default (`adaptive: true`, start 8, max 48).
