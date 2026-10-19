# Hardware requirements

What skim needs, from measurements (the [M7 experiment log](../experiments/m7-gates-and-acquisition.md) has the runs behind each number). Sepolia, October 2026.

## Recommended

| | skim alone (head-only) | skim `--history local` + an indexer (e.g. [smallname](../../examples/smallname)) + Grafana |
|---|---|---|
| **CPU** | 1 core (uses ~0.25-0.5) | 2 cores |
| **Memory** | 512 MB | 2 GB |
| **Disk** | under 100 MB | ~1 GB, growing slowly with history (see below) |
| **Network** | ~0.3 MB/s in, ~0.1 MB/s out, steady | the same |

The measured Linux deployment uses two aarch64 cores. These are sizing recommendations, not enforced process limits; custom networks, larger working sets and heavier RPC traffic need their own measurements.

## Memory

### What was measured

| Setup | RSS | Notes |
|---|---|---|
| Linux (aarch64), `--history local`, 20 subscriptions, default 256-block window, following the head | **179 MB at 26 min** | build `fb4dd7b`, transparent huge pages off; a single uptime sample, not a plateau |
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

`skim_status.memory.parts` (and the Grafana panel "Retained data by structure (stacked) vs RSS") reports instrumented structures' entries, estimated payload bytes and `capacity`. The latter is an allocated collection capacity for some parts, a configured limit or current count for others. Peer payload bytes are currently zero placeholders; network buffers, learned/analyzed-code caches, database page caches, allocator overhead and resident code pages are not fully represented. The sum explains only part of RSS and is not a live-heap measurement.

| Structure | Bound | Filled after (12 s blocks) |
|---|---|---|
| Decoded BALs (`chain.bals_decoded`) | 32 blocks | ~6 min |
| Envelopes and raw BALs kept to serve peers (`cl.envelopes_served`, `chain.bals_raw`) | 64 blocks each | ~13 min |
| Bodies, receipts, withdrawals (`chain.bodies`, `chain.receipts`, `chain.withdrawals`) | 128 blocks each | ~26 min |
| Recent-BAL writes (`state.recent_bal_writes`) | `--window` blocks (256) | ~51 min; ~19.5 MB when full |
| Transaction hashes and index, BAL addresses (`chain.tx_hashes`, `chain.tx_index`, `chain.touched`) | 2,048 blocks | ~7 h |
| Headers (`chain.headers`) | 8,192 blocks | ~27 h |
| Call results (`rpc.call_cache`) | clear triggers: 20,000 entries or estimated 32 MiB | cleared before the next insertion once a trigger is reached; one large entry can exceed 32 MiB |
| EL peers (`el.peers`) | 40 outbound + 15 inbound | minutes |
| Subscribed state (`state.versions`, `state.code`) | `limits.max_subscribed_keys` | as configured |

Fill times assume empty caches and 12 s blocks. Startup seeding and historical requests can fill some caches sooner; missed slots delay them. Other networks' slot durations change the time horizons.

So memory rises for the first hour as the block caches and the window fill, then slowly for many hours (transaction index, headers). Ways to lower it:
- **`--window 32`:** recent-BAL writes and versions shrink with it. Short macOS soaks had ~100 MB medians, with frequent restarts; this is not a Linux or long-run RSS guarantee. Reads at older blocks and reorg depth shrink too.
- **Fewer subscriptions, or a lower `max_subscribed_keys`.**
- **Allocator:** on Linux skim uses mimalloc, which returns memory after a long catch-up (glibc kept 2.4x the live heap). Transparent huge pages are off for the process; `MIMALLOC_ALLOW_THP=1` turns them back on.

## CPU

- **Following the head:** about 0.25 cores head-only. With `--history local`, an indexer's block stream and 20 subscriptions: 0.44 cores on average, 0.53 at the busiest minute (2-core aarch64).
- **Bursts:** catching up history, and cold `eth_call`s that fetch state, use more for a while.
- **Throughput:** `eth_call` reached ~21,000 calls/s at 100 in flight on the development machine with warm state ([M7, Throughput](../experiments/m7-gates-and-acquisition.md#throughput)).

## Disk

- **Head-only (`--history off`):** restart data under `~/.skim/<network>` by default, about 1 MB for the measured subscribed state. `--ephemeral` disables this; an explicit `--archive` still writes its selected file.
- **`--history local`:** `history.redb` holds headers since Glamsterdam plus the log matches block streams asked for: 140 MB on the deployment above. It grows with the chain (headers) and with what modules ask for (matches), not with all logs.
- **`--archive`:** grows with the archived keys' writes ([archive](archive.md)).
- **Monitoring stack:** Prometheus keeps 15 days by default.

Database page-cache budgets also use memory: 8 MiB for restart persistence, 16 MiB for local chain history, and 64 MiB for an enabled selective archive. These are cache budgets, not total database-process costs.

## Network

- **Steady state:** ~0.3 MB/s in and ~0.1 MB/s out on the deployment above (gossip, peers' requests served, SNAP fetches).
- **Ports:** EL `--el-port` (default 30388, TCP and UDP); CL `--cl-port` (default 9130, TCP/libp2p and UDP/discv5), plus port+1 (9131 UDP/QUIC). Inbound connections are optional: skim dials out, preferring QUIC when announced with TCP as fallback. The JSON-RPC port (`--rpc`) should stay on localhost or a private network.
