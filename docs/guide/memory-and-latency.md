# Memory and latency

skim answers fast when the data is already in memory, and slower when it has to fetch it from peers. Most settings move that line: keep more in memory and fewer requests wait on the network, or keep less and pay for it on the misses. This page lists each knob, what it costs and what it buys, then the defaults skim ships with and why.

[Hardware requirements](hardware.md#memory) has the measured footprint, what each structure holds and when it fills, and what `skim_status.memory.parts` does and doesn't count. This page is the decisions.

## Where latency comes from

| An answer comes from | Typical latency | Example |
|---|---|---|
| Memory: subscribed or cached state, a cached call result | milliseconds | 40 warm Universal Resolver calls in one batch: 34-58 ms |
| A recent block's access list (BAL), no fetch | milliseconds | a balance written in the last `--window` blocks |
| Peers: SNAP state, receipts, bodies | 100 ms to seconds per round trip; a cold call needs several | the same 40 calls cold on a fresh node: 6-53 s |
| Peers that aren't there yet | until one connects | restarts that waited 226-245 s for a SNAP peer serving the head |

So the trade is almost always: memory spent keeping something, against network round trips (and peer availability) when it isn't kept.

## The knobs

### State

| Knob | More memory buys | Less memory costs |
|---|---|---|
| **Subscriptions** (`state.subscriptions`) | subscribed keys answer from memory at every block, always current | an unsubscribed key is fetched over SNAP on first use (seconds when cold, longer with few SNAP peers) |
| **`limits.max_subscribed_keys`** (default 100,000) | room for every key your calls need | keys over the limit stay on demand (fetched, kept briefly), and a call subscription that's cut short reports `truncated` and isn't ready |
| **`on_demand.cache_ttl_blocks`** (default 4) | unsubscribed state stays current longer after use, so repeated reads stay warm | a key read again after it expired is fetched again |
| **`limits.max_on_demand_keys`** (default 200,000) | more on-demand keys kept at once | the soonest-expiring are evicted first and fetched again on the next read |
| **`--window`** (default 256 blocks) | reads at older recent blocks; deeper reorgs handled in memory; keys written within the window resolve from BALs without a fetch (also right after a restart: up to 32 blocks' BALs are seeded) | ~16–19.5 MB of recent-BAL writes in the recorded 256-block workloads, plus versions. Short macOS soaks with `--window 32` had ~100 MB medians; older reads need a fetch or the archive |
| **Predictors** (`predictors.uniswap_v3.prefetch_budget`, `window_ttl_blocks`) | ticks near the current price are warm before anyone asks | quotes that cross into cold ticks fetch them on demand |

### Results and block data

| Knob | More memory buys | Less memory costs |
|---|---|---|
| **Call result cache** (on by default; `--no-call-cache` turns it off; clear triggers: 20,000 entries or estimated 32 MiB) | an identical `eth_call` at the same block answers without executing | every call executes (still from memory if its state is warm; CPU, not network) |
| **Block caches** (bodies, receipts, withdrawals: 128 blocks; decoded BALs: 32; raw BALs and envelopes kept to serve peers: 64) | `eth_getLogs`, receipts, transaction replay and log scans over recent blocks need no fetch; peers syncing from skim get answers | requests for older blocks fetch from peers (verified against the header) |
| **Transaction index** (`--tx-index`, default `subscribed`; `--tx-index-blocks`, default 2,048, 0 = none) | `--tx-index all`: `eth_getTransactionByHash`, `eth_getTransactionReceipt` and `debug_traceTransaction` find any transaction from the last ~7 hours (wallet backends, explorers) | by default only transactions whose state changes touch a subscribed address (an address with any subscribed key; covers ones it sent) are found, plus ones sent through skim; read-only interactions aren't indexed. Unindexed lookups return `null`. Entries are 8-byte hash prefixes, confirmed against the transaction on lookup |
| **`--history local`** | linked post-fork headers and fetched matching logs on disk, so covered queries can outlive peer receipt retention | disk (140 MB measured) plus a 16 MiB database page-cache budget; unfetched coverage still needs peers |

The block-cache sizes are compile-time constants today (the transaction index has its flags); `skim_status.memory` shows what they hold.

The call cache checks its triggers before insertion. A single large result can push estimated bytes over 32 MiB; the next insertion clears it. Cached values include the result/revert and a list of read addresses, not a list of every storage key. Restarts clear this cache, so aggregate call counts across a soak are not simultaneous cache occupancy.

### Network

| Knob | More memory buys | Less memory costs |
|---|---|---|
| **EL peer limits** (40 outbound + 15 inbound) | more peers to fetch from, so a SNAP peer that serves the head is more likely to be connected | each session holds buffers and peer state |
| **Restart snapshot** (`--data-dir`, the default; `--ephemeral` keeps none) | after a restart, subscribed state is back within seconds (replayed to the head and verified) | starts empty; every key is fetched again |

### Allocator and pages (Linux)

| Knob | Effect |
|---|---|
| **Transparent huge pages** (off; `MIMALLOC_ALLOW_THP=1` turns them on) | Huge pages map memory in 2 MiB pages instead of 4 KiB, so the CPU's address-translation cache covers more memory. That helps programs that sweep large working sets. skim's heap is a few hundred MB of short-lived, scattered allocations: a 2 MiB page with one live object stays fully resident, so RSS is higher with them (measurements under [Defaults](#defaults-and-why)). The speed benefit for skim hasn't been measured. |
| **Allocator** | mimalloc on Linux (returns freed memory after long catch-ups), the system allocator on macOS (smaller there). Built in, not a runtime switch. |

## Choosing

- **Small machine (512 MB-1 GB):** the defaults, plus whichever of these fit; expect cold reads to wait on peers.
  - **`--window 32`:** recent-BAL writes and versions shrink with it, and so do reads at older blocks and reorg depth. Short macOS soaks had ~100 MB medians, with frequent restarts; this is not a Linux or long-run RSS guarantee.
  - **`--tx-index-blocks 0`:** no transaction lookups by hash except transactions sent through skim. An indexer that only reads logs and state needs none.
  - **Subscriptions only for what you read every block,** or a lower `max_subscribed_keys`.
  - **Allocator:** keep the defaults (mimalloc on Linux, huge pages off); see [Defaults](#defaults-and-why).
- **Lowest latency for a known workload:** subscribe what your calls read (the dashboard's "Addresses your calls read" lists what isn't subscribed yet, with a ready-made `skim.yaml` block), keep the default window and call cache. Subscribed state is small next to the block caches (about 100 bytes per key plus a version per change within the window): watch `state.versions` in `skim_status.memory` as you add keys.
- **History and an indexer:** `--history local` stores headers and fetched matching logs on disk with a bounded 16 MiB page cache. Run the indexer on the same machine so its block stream stays local.
- **Wallet backend or explorer looking up arbitrary transactions by hash:** `--tx-index all`. Its cost is in the [defaults table](#defaults-and-why).
- **A deployment that must restart fast:** keep `--data-dir`, and subscribe rather than rely on on-demand keys. A key that's new after a restart, and not in a recent BAL, needs a SNAP peer, and those can take minutes to appear.

## Defaults, and why

Each default below records what was chosen, the evidence, and what was turned down. Revisit one when its evidence changes.

| Decision | Chosen | Evidence | Turned down |
|---|---|---|---|
| **Huge pages** | off on Linux (`MIMALLOC_ALLOW_THP=1` to enable) | production, same state and uptime: 179 MB without against 251-256 MB with (about 29% less) | on: a speed gain for skim is unmeasured; to be revisited with a `skim bench-node` A/B |
| **Allocator** | mimalloc on Linux, system allocator on macOS | Linux catch-up: glibc 394-610 MB RSS against a 228 MB live heap, mimalloc 230-296 MB. macOS head-only: system allocator footprint 86-94 MB against mimalloc's 236-255 MB | one allocator everywhere: each is the larger one on the other platform |
| **Allocator tuning** | none besides huge pages | eager arena commit off plus purge delay 0: no change (241 against 238-251 MB at 20 min) | |
| **`--window`** | 256 blocks | covers about 51 minutes of reads and reorgs; its recent-BAL writes are ~19.5 MB when full | 32 for everyone: halves head-only RSS, but reads at older recent blocks then need a fetch or the archive |
| **Startup BAL seeding** | the last 32 blocks before the first applied one | a key subscribed at restart for an account that just transacted resolved without SNAP; 31 blocks load in 1.7-3.7 s. The whole 256-block window took 32 MB of payloads to fetch and parse at once (RSS 245 MB within 5 min) | the full window |
| **Call result cache** | 20,000 entries or estimated 32 MiB trigger a clear before the next insertion; a single result can overshoot | entries are keyed by block hash; the count alone didn't bound variable-size results | no cache: repeat calls re-execute; an entry-count-only bound |
| **EL peer limits** | 40 outbound + 15 inbound | skim reached 30-42 sessions; reth's larger defaults (100 + 30) permit more session buffers and peer state | reth's defaults; a tighter cap risks fewer SNAP peers, which is what slow restarts waited for |
| **Fetch order** | completion order (each account's code requested as soon as the account arrives) | no measurable latency difference from the older wave order on 40 cold calls (peer variance dominated, 6-53 s); about 15% more code requests | the wave order; batching codes that arrive close together, if code requests start to matter |
| **Block caches** | 128 / 64 / 32 blocks | not tuned yet: the per-structure counters now measure what each costs | shorter caches, which would make more recent-block requests fetch from peers |
| **Transaction index** | transactions of subscribed addresses only, 2,048 blocks | at 9 h, indexing every transaction held ~21 MB of payload (hash index plus per-block hash lists), an estimated 40-50 MB of RSS at the measured ~2.3x overhead, mostly for transactions no app asked about | `all` as the default; it stays available for explorers and wallet backends |
| **Peer history and dashboard tables** | bounded: 4,096 peer records, 1,024 dial cooldowns, 65,536 seen nodes, unknown RPC methods counted as `other` | each grew with every peer seen or every distinct method name (review findings) | unbounded |

Long runs: memory levels off once the ~7 h caches are full ([hardware](hardware.md#long-runs): 346 MB at 9 h with every transaction indexed, +3 MB over the last two hours). Headers keep filling slowly until ~27 h; a run on the `--tx-index subscribed` build past 27 h is pending.
