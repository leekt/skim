# Hardware requirements

What skim needs, from measurements (the [M7 experiment log](../experiments/m7-gates-and-acquisition.md) has the runs behind each number). Sepolia, October 2026.

## Recommended

| | skim alone (head-only) | skim `--history local` + an indexer (e.g. [smallname](../../examples/smallname)) + Grafana |
|---|---|---|
| **CPU** | 1 core (uses ~0.25-0.5) | 2 cores |
| **Memory** | 512 MB | 2 GB |
| **Disk** | under 100 MB | ~1 GB, growing slowly with history (see below) |
| **Network** | ~0.3 MB/s in, ~0.1 MB/s out, steady | the same |

An Oracle Cloud Ampere A1 (2 cores, 12 GB) runs the whole right-hand column with most of its memory free.

## Memory

### What was measured

| Setup | RSS | Notes |
|---|---|---|
| Linux (aarch64), `--history local`, 20 subscriptions, default 256-block window, following the head | **179 MB at 26 min** | the current build: transparent huge pages off |
| same, with transparent huge pages on (builds before `fb4dd7b`) | 251-256 MB at 25-28 min | mimalloc asked for 2 MiB pages; a partly used one counts fully |
| macOS, head-only, `--window 32`, soak load, restarts every 6-12 min | median 99-105 MB, peaks 168-194 MB | peaks are the catch-up right after a restart |
| Linux, catching up ~40k blocks of history | 230-296 MB | before the huge-page change; glibc's allocator instead: 394-610 MB |

**Over three hours** (production, Linux aarch64, `--history local` plus an indexer, 20 subscriptions, default window, huge pages off), RSS in MB against the sum of what `skim_status.memory` counts:

| Minutes | 1 | 31 | 61 | 106 | 136 | 181 |
|---|---|---|---|---|---|---|
| RSS | 109 | 203 | 240 | 262 | 265 | 276 |
| Counted structures | 4 | 46 | 61 | 62 | 63 | 66 |

- **Plateaued within about an hour:** block caches (receipts ~10-11 MB, bodies ~8 MB, envelopes served ~7 MB, raw BALs ~3 MB), recent-BAL writes (~16-18 MB), EL peers (~38).
- **Still growing, as designed:** the transaction index, transaction hashes and BAL addresses, ~1.5 MB per 15 minutes together until 2,048 blocks (~7 h; about 19 MB more), and headers until ~27 h (a few MB).
- **Not yet explained:** the gap between RSS and the counted structures (allocator overhead, code pages, network buffers, anything uncounted) grew from ~200 MB to ~210 MB over the last 75 minutes. Slow, but not flat: whether memory levels off is settled only by a run longer than the slowest cache's fill time (7 h; then 27 h).

### What holds memory

How to trade memory for latency, knob by knob, and why the defaults are what they are: [memory and latency](memory-and-latency.md).

`skim_status.memory` (and the Grafana panel "Retained data by structure vs RSS") reports, for every retained structure, its entries, an estimate of the payload bytes it holds, and its allocated capacity. Payload only: allocator overhead and code pages aren't counted, so the sum explains part of RSS, never all of it. On Linux, roughly 30-60 MB of RSS is the binary's code and shared libraries.

| Structure | Bound | Filled after (12 s blocks) |
|---|---|---|
| Decoded BALs (`chain.bals_decoded`) | 32 blocks | ~6 min |
| Envelopes and raw BALs kept to serve peers (`cl.envelopes_served`, `chain.bals_raw`) | 64 blocks each | ~13 min |
| Bodies, receipts, withdrawals (`chain.bodies`, `chain.receipts`, `chain.withdrawals`) | 128 blocks each | ~26 min |
| Recent-BAL writes (`state.recent_bal_writes`) | `--window` blocks (256) | ~51 min; ~19.5 MB when full |
| Transaction hashes and index, BAL addresses (`chain.tx_hashes`, `chain.tx_index`, `chain.touched`) | 2,048 blocks | ~7 h |
| Headers (`chain.headers`) | 8,192 blocks | ~27 h |
| Call results (`rpc.call_cache`) | 20,000 entries and 32 MiB | depends on traffic; cleared when either is reached |
| EL peers (`el.peers`) | 40 outbound + 15 inbound | minutes |
| Subscribed state (`state.versions`, `state.code`) | `limits.max_subscribed_keys` | as configured |

So memory rises for the first hour as the block caches and the window fill, then slowly for many hours (transaction index, headers). Ways to lower it:
- **`--window 32`:** the recent-BAL writes and versions shrink with it (head-only RSS near 100 MB). Reads at older blocks and reorg depth shrink too.
- **Fewer subscriptions, or a lower `max_subscribed_keys`.**
- **Allocator:** on Linux skim uses mimalloc, which returns memory after a long catch-up (glibc kept 2.4x the live heap). Transparent huge pages are off for the process; `MIMALLOC_ALLOW_THP=1` turns them back on.

## CPU

- **Following the head:** about 0.25 cores head-only. With `--history local`, an indexer's block stream and 20 subscriptions: 0.44 cores on average, 0.53 at the busiest minute (2-core aarch64).
- **Bursts:** catching up history, and cold `eth_call`s that fetch state, use more for a while.
- **Throughput:** `eth_call` reached ~21,000 calls/s at 100 in flight on the development machine with warm state ([M7, Throughput](../experiments/m7-gates-and-acquisition.md#throughput)).

## Disk

- **Head-only (`--history off`):** a restart snapshot of subscribed state, about 1 MB. `--ephemeral` keeps nothing.
- **`--history local`:** `history.redb` holds headers since Glamsterdam plus the log matches block streams asked for: 140 MB on the deployment above. It grows with the chain (headers) and with what modules ask for (matches), not with all logs.
- **`--archive`:** grows with the archived keys' writes ([archive](archive.md)).
- **Monitoring stack:** Prometheus keeps 15 days by default.

## Network

- **Steady state:** ~0.3 MB/s in and ~0.1 MB/s out on the deployment above (gossip, peers' requests served, SNAP fetches).
- **Ports:** EL `--el-port` (default 30388, TCP and UDP) and CL `--cl-port` (default 9130, TCP and UDP) for peers. Inbound connections are optional: skim dials out. The JSON-RPC port (`--rpc`) should stay on localhost or a private network.
