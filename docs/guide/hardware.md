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

This section is the measurements and what fills when. Which knobs trade memory for latency, and why the defaults are what they are: [memory and latency](memory-and-latency.md).

### Zero config: no subscriptions, head-only
`skim` with no config file (nothing pinned, `--history off`, default 256-block window), following Sepolia over public peers on macOS (Apple silicon), build `c704325` (run as `skim run` with no config; same behaviour as plain `skim` since `e899046`), 2026-10-19. A fresh data dir; a few read-only example scripts queried it in the first half hour, otherwise idle.

| Minutes | 1 | 5 | 10 | 20 | 30 | 40 | 50 | 60 |
|---|---|---|---|---|---|---|---|---|
| RSS (MB) | 69 | 112 | 131 | 149 | 161 | 170 | 175 | 179 |
| Downloaded (MB, cumulative) | 19 | 83 | 152 | 289 | 402 | 526 | 643 | 757 |
| EL peers | 0 | 3 | 4 | 10 | 16 | 18 | 19 | 25 |

- **Startup:** the first SNAP-serving peer connected after 10.8 s, the first block was applied within the first minute, and RPC answered from then on.
- **Memory:** 179 MB at one hour, still rising slowly (+5 MB over the last 10 minutes, against +18 MB per 10 minutes in the first). skim's counted structures were 52 MB: recent-BAL writes 16, bodies 14, envelopes kept to serve peers 10.5, raw BALs 3.4. The window and block caches are full by ~51 minutes; BAL address sets keep filling until ~7 h and headers until ~27 h, so this is not a steady state. A 43-hour Linux soak in the same mode is under [long runs](#long-runs).
- **CPU:** 13 min 43 s of CPU in 61 minutes: about 0.22 of a core on average.
- **Network:** 152 MB in and 35 MB out in the first 10 minutes, then about 720 MB/h in and 170 MB/h out (~17 GB/day in, ~4 GB/day out), mostly gossip and serving peers; it grew with the peer count.
- **Disk:** 184 KB (restart data only).

### Short runs

| Setup | RSS | Notes |
|---|---|---|
| Linux (aarch64), `--history local`, 20 subscriptions, default 256-block window, following the head | **179 MB at 26 min** | build `fb4dd7b`, transparent huge pages off; a single uptime sample, not a plateau |
| same, with transparent huge pages on (builds before `fb4dd7b`) | 251-256 MB at 25-28 min | mimalloc asked for 2 MiB pages; a partly used one counts fully |
| macOS, head-only, `--window 32`, soak load, restarts every 6-12 min | median 99-105 MB, peaks 168-194 MB | peaks are the catch-up right after a restart |
| Linux, catching up ~40k blocks of history | 230-296 MB | before the huge-page change; glibc's allocator instead: 394-610 MB |

### Long runs

Both runs below are the production deployment: Linux aarch64, `--history local` plus an indexer, 20 subscriptions, default window, on builds that indexed every transaction (before `--tx-index pinned` became the default). "Counted structures" is the sum of what `skim_status.memory` counts.

**Three hours** (huge pages off):

| Minutes | 1 | 31 | 61 | 106 | 136 | 181 |
|---|---|---|---|---|---|---|
| RSS (MB) | 109 | 203 | 240 | 262 | 265 | 276 |
| Counted structures (MB) | 4 | 46 | 61 | 62 | 63 | 66 |

- **Plateaued within about an hour:** block caches (receipts ~10-11 MB, bodies ~8 MB, envelopes served ~7 MB, raw BALs ~3 MB), recent-BAL writes (~16-18 MB), EL peers (~38).
- **Still growing, as designed:** the transaction index, transaction hashes and BAL addresses, ~1.5 MB per 15 minutes together until 2,048 blocks (~7 h), and headers until ~27 h (a few MB).
- At three hours the gap between RSS and the counted structures was still growing (~200 MB to ~210 MB over the last 75 minutes); the nine-hour run explains it.

**Nine hours** (the same process continued; build `fb4dd7b`, huge pages off):

| Hours | 0 | 3 | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|
| RSS (MB) | 147 | 281 | 335 | 343 | 344 | 346 |

- **It levels off with the caches.** The counted structures reached ~91 MB by ~6-7 h and stopped growing; RSS went flat with them (+3 MB over the last two hours). Over the run RSS grew about 2.3 MB per MB of counted payload: the counters count keys and values, not hash-table slots, spare capacity or allocator rounding, so the uncounted part grows with the caches rather than on its own.
- **Where it is:** 305 of 339 MB was mimalloc's heap (skim's own allocations), 22 MB the binary, nothing else of note.
- **The largest growing group was the transaction index** (index, per-block hash lists and BAL addresses: ~36 MB of payload). Since `9ed1f49`, transactions are indexed only if they change a pinned address (`--tx-index pinned`, the default), the index stores 8-byte hash prefixes, and the per-block lists hold those 8-byte keys instead of full hashes. Headers still fill until ~27 h (a few MB).

**27 hours on the pinned-index build** (`9ed1f49`, mimalloc, the same deployment): RSS rose to ~370-400 MB by ~9 h and stayed there through 27.7 h (379 MB, peak 412). The production indexer's load is what puts it above the zero-config run below.

**43 hours, zero config** (Linux aarch64, no subscriptions, head-only, mimalloc build `57478b6`; sampled every minute):

| Hours | 3 | 6 | 12 | 18 | 24 | 30 | 36 | 43 |
|---|---|---|---|---|---|---|---|---|
| RSS (MB) | 225 | 268 | 266 | 274 | 282 | 290 | 295 | 307 |
| Live heap counter (MB) | 52 | 56 | 53 | 58 | 51 | 57 | 63 | 62 |

- **Peak RSS:** 314 MB. CPU: 29,500 s over 43.6 h, 0.19 of a core. EL peers 26-40, SNAP-serving 10-33.
- **Most growth is in the first 6 hours** (block caches and BAL address sets). After that RSS creeps ~1 MB per hour.
- **What still grows is bounded:** headers fill toward 8,192 (1.5 MB at 6 h, 7.1 MB at 43 h; full at ~27 h), BAL address sets +3 MB, bodies +2 MB. Counted structures went from 56 to 65 MB between 12 and 43 h while RSS went from 266 to 307 MB. The rest is allocator overhead.
- **Allocators compared** (three builds of `57478b6` side by side on the same machine with production's config, 11.3 h): mimalloc (the default) 244 MB, glibc 229 MB, jemalloc 208 MB. jemalloc also used ~30% less CPU. Build it with `--features alloc-jemalloc`.

**Production on jemalloc** (`df686b7` with `--features alloc-jemalloc`, from 2026-10-20 15:20 UTC): RSS 296 MB at 20 h (peak 321), against ~380-400 MB on mimalloc under the same load.

### What holds memory

`skim_status.memory.parts` (and the Grafana panel "Retained data by structure (stacked) vs RSS") reports instrumented structures' entries, estimated payload bytes and `capacity`. The latter is an allocated collection capacity for some parts, a configured limit or current count for others. Peer payload bytes are currently zero placeholders; network buffers, learned/analyzed-code caches, database page caches, allocator overhead and resident code pages are not fully represented. The sum explains only part of RSS and is not a live-heap measurement.

| Structure | Bound | Filled after (12 s blocks) |
|---|---|---|
| Decoded BALs (`chain.bals_decoded`) | 32 blocks | ~6 min |
| Envelopes and raw BALs kept to serve peers (`cl.envelopes_served`, `chain.bals_raw`) | 64 blocks each | ~13 min |
| Bodies, receipts, withdrawals (`chain.bodies`, `chain.receipts`, `chain.withdrawals`) | 128 blocks each | ~26 min |
| Recent-BAL writes (`state.recent_bal_writes`) | `--window` blocks (256) | ~51 min; ~19.5 MB when full |
| Transaction index (`chain.tx_index`, `chain.tx_blocks`): pinned transactions only by default | `--tx-index-blocks` (2,048) | ~7 h; small unless `--tx-index all` |
| Transactions sent through skim (`chain.sent_txs`, `chain.sent_included`) | pending: re-broadcast for up to 30 min; included: 24 h, at most 4,096 | as sent |
| BAL addresses per block, an `eth_getLogs` pre-filter (`chain.touched`) | 2,048 blocks | ~7 h |
| Headers (`chain.headers`) | 8,192 blocks | ~27 h |
| Call results (`rpc.call_cache`) | clear triggers: 20,000 entries or estimated 32 MiB | cleared before the next insertion once a trigger is reached; one large entry can exceed 32 MiB |
| EL peers (`el.peers`) | 40 outbound + 15 inbound | minutes |
| Subscribed state (`state.versions`, `state.code`) | `limits.max_subscribed_keys` | as configured |

Fill times assume empty caches and 12 s blocks. Startup seeding and historical requests can fill some caches sooner; missed slots delay them. Other networks' slot durations change the time horizons.

So memory rises for the first hour as the block caches and the window fill, then slowly until ~7 h (BAL addresses, and the transaction index with `--tx-index all`) and a little until ~27 h (headers). To lower it (`--window`, `--tx-index-blocks`, subscriptions, allocator settings), see [memory and latency](memory-and-latency.md#choosing).

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
