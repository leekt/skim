# P3: skim and a full node, same workload, same block hashes

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Run on 2026-10-15 (KST), Sepolia, one machine (Apple Silicon, home connection).
- **Full node:** Nethermind 2.1.0 (full, not archive) with Lighthouse, synced before the run.
- **skim:** `skim run --config examples/uniswap-v3/skim.yaml`, excluding the local Nethermind as a SNAP peer, so every state fetch went to public peers.

Driver: `skim bench-node`.
- **Workload:** every block, 15 `eth_call`s at that block's hash. That's 7 fixed calls (balances, V3 pool reads, quotes) plus 8 random new-input V3 quotes.
- **Concurrency sweep:** every 50 blocks, at 1, 10 and 100 in flight.

**Phases:**

| Phase | Nodes running | Blocks | Duration |
|---|---|---|---|
| A | full node alone | 300 | ~1 h |
| B | both side by side, same calls on both | 300 | ~1 h |

**Harness note:** phase B first ran without waiting for each endpoint to have the block. skim, which often sees a block a few hundred ms after the full node, answered "unavailable" for those, so the run was stopped. The rerun waits until each endpoint has the block before calling. Data: `data/p3/`.

## Results
### Correctness (phase B)
| | Result |
|---|---|
| Calls compared at the same block hash | **4,500** |
| Identical results | **4,500** |
| Different results | **0** |
| Errors (either side) | 0 |

### Latency and throughput (phase B, local HTTP)
| | skim | full node |
|---|---|---|
| `eth_call` P50 / P95 / P99 | 0.77 / 3.6 / 3.9 ms | 0.30 / 2.8 / 3.1 ms |
| Throughput, 1 in flight | 1,195 calls/s | 2,467 calls/s |
| Throughput, 10 / 100 in flight | 3,697 / 3,595 calls/s | 17,633 / 18,909 calls/s |
| Block available after its timestamp, P50 / P90 / P99 | 845 / 1,374 / 3,071 ms | 793 / 1,324 / 2,912 ms |
| Lag behind the other endpoint for the same block, P50 / P90 / P99 | 0 / 253 / 441 ms | 0 / 0 / 0 ms |

### Resources (phase B, sampled every 60 s)
| | skim | full node (Nethermind) | consensus client (Lighthouse) |
|---|---|---|---|
| Memory (RSS) P50 / max | 163 / 188 MB | 6.9 / 7.0 GB | 534 / 646 MB |
| CPU P50 / P90 | 10% / 18% of a core | 854% / 887% ¹ | 7% / 24% |
| Network in / out | 189 / 32 kB/s | 127 / 961 kB/s ² | 710 / 174 kB/s |
| Disk | 0 | 342 GB (+4.4 GB during the run) | 12.7 GB |

¹ The full node used ~8.5 cores throughout both phases, with or without skim running. Our call workload doesn't explain it (it peaks during the brief sweeps). It is most likely database maintenance after the catch-up, but we didn't verify that. A steady-state run of several hours is needed before this number means anything.

² Includes serving SNAP and other data to its own peers.

## What this does and doesn't show
- **Same answers:** every one of the 4,500 calls returned the same result from both at the same block hash, including 2,400 never-seen quotes. skim fetched their state from public peers.
- **Execution:** the full node's local execution is faster and scales further with concurrency. skim levels off around 3,600 calls/s. That hasn't been profiled yet.
- **Freshness:** skim usually has a block at the same time as the full node, and up to ~0.5 s later in the tail. That comes from its position in the consensus gossip mesh, which a later change addresses (wider mesh, more peers).
- **Not shown:** behaviour under restarts, reorgs and key-set changes, and long-run memory bounds. That is the next milestone's soak.
