# Memory audit (2026-10-18)

A source audit of skim's memory use at `47ceff4`, with the A/B measurements it was based on. Its measurements and recommendations are historical.

## Current status (2026-10-19)

Since the audit, current code has platform-specific allocators (mimalloc on Linux, system allocator on macOS), transparent huge pages disabled by default on Linux, 40 outbound / 15 inbound EL peer targets, per-structure memory estimates in `skim_status.memory.parts` and Grafana, and call-cache clear triggers at 20,000 entries or an estimated 32 MiB. A large result can exceed the byte trigger until the next insertion. Cached read lists contain addresses, not individual storage keys. Chain-data caches and the history database now live in `skim-chain`. The four cleanup findings below were applied (see "Review of the audit").

**Current measurements:** [hardware requirements](../guide/hardware.md#memory) and [memory and latency](../guide/memory-and-latency.md). In short: production, Linux aarch64, `--history local` plus an indexer, on the build that indexed every transaction, RSS was 147 MB at start, 281 MB at 3 h, 335 MB at 6 h and 346 MB at 9 h, flat after about 7 h (+3 MB over the last 2 h). The counted structures reached ~91 MB; RSS grew about 2.3x the counted payload (hash-table and allocator overhead), and 305 of 339 MB was mimalloc's heap. Whether RSS stays flat once headers fill (~27 h) is [TODO 6](../TODO.md#memory), now measured on the `--tx-index pinned` build.

Short macOS soaks restart every 6–12 minutes and use `--window 32`: their ~100 MB medians cannot be attributed to full seven-hour transaction indexes. All in-process caches reset at restart. Payload estimates exclude allocation slack, code pages and several uninstrumented structures, so they are not a complete live-heap breakdown.

## Scope

Status as of 2026-10-18 (`main` at `47ceff4`). The original handoff treats ~230 MB RSS as
acceptable and prioritizes finding a **leak**. The follow-up source audit below also records
memory-reduction options requested by the owner. These are recommendations; no implementation
changes or new long-running soak were made during the audit.

## Review of the audit, and what was applied
Every finding under "Missing cleanup and unbounded retention" was checked against the source and
holds. Applied, with regression tests; 158 workspace tests pass.

| Finding | Change | Test |
|---|---|---|
| 1. SNAP/1 pending survives caller timeout | `snap1.rs`: queued commands whose caller gave up aren't sent; abandoned `pending` entries are pruned on every send | None (needs a live RLPx connection); `ProtocolConnection` has no test seam |
| 2. Peer history never evicts | CL `last_dial` drops expired cooldowns past 1,024 entries; `seen_nodes` resets at 65,536 (`discovered` now counts distinct nodes since the last reset); EL `PeerBook` keeps at most 4,096 records, dropping crawlers and peers that only dropped us first. Aggregate session and close counts are unchanged | `crawler_churn_doesnt_grow_the_book_or_evict_good_peers` |
| 3. Dashboard method names and `record_hit` | Unknown methods are recorded as `other` (as in metrics); `record_hit` prunes like `record_call` | `cache_hits_on_many_addresses_stay_bounded` |
| 4. Scheduler: orphaned jobs, starved background work | Jobs whose callers all gave up are dropped unless in flight (`stats.orphaned`); closed waiters are pruned. An idle peer may take one background request when no foreground job waits, so a cap of 1 no longer starves background work | `a_job_nobody_waits_for_is_dropped_not_sent`, `background_work_runs_on_a_peer_with_a_cap_of_one`; both mutation-checked |

From the reductions table:
- **Allocator:** mimalloc on Linux only. macOS uses the system allocator, which was ~80-110 MB smaller in the A/B.
- **Startup seeding:** the batch keeps only writes from the moment each BAL arrives.
- **Restart replay:** `skim_state::persist::replay` iterates borrowed writes, so `verify_and_replay` no longer clones the batch.

Deferred at that review (several items were implemented later; see current status above):
- **A ceiling on never-sent scheduler jobs while peers exist.** Long background backlogs are
  legitimate (`queueing_for_capacity_is_not_a_deadline`), and the starvation case is fixed directly
  instead.
- **A bound on the SNAP/1 command queue.** The scheduler's per-peer in-flight caps already bound
  what's queued.
- **Cache horizons and byte budgets, `--window`, new `skim_status` cache counters.** These are
  tuning and instrumentation, recommended steps 3 and 5, left for when lower RSS is wanted.

At that review, these fixes had not been re-measured as RSS. Later measurements: see current status
above; whether RSS stays flat past 9 h is still open (TODO 6). The applied fixes remove unbounded growth that needs timeouts,
peer churn, or unusual requests to show up.

## Historical numbers (head-only, `--history off`)
- 150-min soak, `be3f0fa` (macOS): RSS median 181 MB, max 263 MB. Restarts every 6-12 min.
- 60-min side-by-side run, no restarts, same load (macOS):

| Build | 10 min | 20 min | 30 min | 38 min | 60 min max |
|---|---|---|---|---|---|
| `5237bc1` (system allocator) | 110 | 135 | 149 | 161 | 173 |
| `be3f0fa` without mimalloc | 104 | 130 | 145 | 155 | 172 |
| `be3f0fa` (mimalloc) | 186 | 217 | 243 | 266 | 287 |
| current (mimalloc) | 192-211 | 238-251 | 261 | 273 | 301 |
| current without mimalloc | 129 | 149 | - | - | - |
| current, `MIMALLOC_ARENA_EAGER_COMMIT=0 MIMALLOC_PURGE_DELAY=0` | 210 | 241 | - | - | - |

- On aarch64 Linux, following the head with mimalloc: 181 MB after 10 min. Catching up ~40k
  blocks: 230-296 MB, against 394-610 MB with glibc.

## Why RSS went from ~134 MB to ~230+ MB (measured)
1. **mimalloc (`106e3dc`): +80-110 MB.**
   - Same code with the system allocator stays at the old figures.
   - macOS `vmmap --summary` at 57 min: with the system allocator, live heap 103 MB (malloc zone
     "bytes allocated"), physical footprint 86-94 MB. With mimalloc, footprint 236-255 MB, with the
     same state and allocation counts. So it's memory mimalloc retains after freeing, not more live
     data.
   - The env tuning above made no difference. The bundled version is mimalloc v2.3.2
     (libmimalloc-sys 0.1.49), and its purge delay is already 10 ms by default.
   - mimalloc was adopted because glibc's per-thread arenas made catch-up RSS 2.4x the live heap
     on Linux (comment in `crates/skim-cli/src/main.rs`).
2. **The 256-block window filling (`--window`, default 256 = ~51 min).**
   - The recent-BAL index (~19.5 MB of writes at 256 blocks) and the key versions fill during
     roughly the first hour. Other caches retain more than 256 blocks and continue filling
     afterwards; see the follow-up audit below.
   - Soak segments are only 7-13 min, so soak peaks depend on uptime.
3. **Changes since `be3f0fa`: about +20 MB.**
   - Without mimalloc, the current build is ~20 MB above `be3f0fa` at equal uptime (129 vs 104 at
     10 min).
   - Still not fully attributed. Startup BAL seeding (`persist::seed_recent_bals`, 32 envelopes)
     adds a few MB of recent writes before that window would otherwise fill, plus temporary
     fetch/decode allocations. More threads were observed (26 vs 15-16), but their contribution
     has not been isolated. Atom-coverage history code does not explain this head-only workload:
     `--history off` never installs the history database.

## Open question from the earlier audit
- **Cause of the observed growth.** The audit found paths with missing cleanup, but has not
  shown that they caused the recorded RSS curve. The 60-min runs grew until the end (system
  allocator 155 → 172 MB between 38 and 57 min). The state window filling is only part of the
  explanation: transaction/address indexes can fill for ~7 hours and headers for ~27 hours.
- **To check now:** the other caches and pending-work sizes are exposed. Run without restarts
  and compare RSS/live heap with those allocations and the uninstrumented remainder. A 3-4 hour run is useful for measuring
  growth, but cannot establish that all default caches have plateaued. Use reduced cache limits
  in a diagnostic build to exercise eviction sooner, or run past the relevant fill horizons.
  - Run a system-allocator build as well. Its live heap is visible to `vmmap`/`heap`; mimalloc's
    isn't.
  - Watch `skim_status.store`: `keys`, `versions`, `recent_bals`, `recent_bal_bytes`,
    `on_demand_keys`, `code_bytes`. Flat store counters alone do not establish a leak: they omit
    chain-data caches, call results, learned access sets, and collection allocation capacity.
  - Also watch the peer count (`el_peers`, CL `peers`). Per-peer buffers could grow with peer
    churn.

## Follow-up source audit: consumers and reductions

Evidence: existing A/B measurements, source inspection at `47ceff4`, and decoding nine local
SSZ envelope fixtures. Cache sizes extrapolated from fixtures are estimates, not a new heap
profile or measured RSS savings. The additional ~20 MB since `be3f0fa` remains unassigned.

### Bounded caches keep filling beyond the state window

At one block every 12 seconds, starting with empty caches:

| Cache | Default limit | Approximate fill time |
|---|---:|---:|
| Decoded BALs | 32 blocks | 6.4 min |
| Served CL envelopes and raw BALs | 64 blocks each | 12.8 min |
| Bodies and withdrawals | 128 blocks each | 25.6 min |
| State recent BALs | 256 blocks | 51.2 min |
| Transaction indexes and touched addresses | 2,048 blocks | 6.8 h |
| Call results | 20,000 entries | 13.3 h at the driver's 5 distinct, cacheable calls/block |
| Headers | 8,192 blocks | 27.3 h |

Missed slots, startup seeding, historical requests, and cacheable-result rates change these
times. Call results clear at their entry limit, so their occupancy can rise and fall rather
than plateau. See [chain-data cache limits](../../crates/skim-chain/src/chaindata.rs),
[call-result cache](../../crates/skim-cli/src/rpc.rs), and
[CL envelope cache](../../crates/skim-cl/src/serve.rs).

The nine fixtures are `data/fuzz-seeds/envelope-11363645-{0..7}.ssz` (local, gitignored) and
`crates/skim-cl/tests/fixtures/sepolia-11356920.envelope.ssz`. They average 123.6 transactions
and 328.1 touched addresses per block. The transaction-hash vector, transaction lookup index,
and address entries have a payload lower bound of:

```
123.6 * (32-byte transaction hash + 48-byte lookup entry) + 328.1 * 20-byte address
  ~= 16,447 bytes/block
  ~= 32.1 MiB at 2,048 blocks
```

This excludes spare hash-table capacity, control bytes, containers, and allocator overhead;
actual allocated memory is larger. At the same sample mix, the three caches of raw payload
data alone would retain roughly 7.8 MiB of full SSZ envelopes, 2.8 MiB of raw BALs, and 9.6 MiB
of transaction bytes, before decoded objects and indexes. These are overlapping representations
of the same blocks, explicitly copied in `run.rs`, rather than accidental backing-buffer pins.

| Consumer | Reduction | Tradeoff or limitation |
|---|---|---|
| Unconditional mimalloc in `skim-cli/src/main.rs` | Use the system allocator on macOS; keep mimalloc on Linux | Highest measured macOS opportunity (~80-110 MB in the existing A/B); Linux measurements favor mimalloc |
| Transaction/address indexes in `chaindata.rs` | Make the 2,048-block limit configurable; replace touched-address hash sets with sorted vectors and binary search | Shorter retention reduces recent transaction lookup and BAL-based log filtering coverage; sorted vectors preserve membership queries |
| Payload caches in `chaindata.rs` and `skim-cl/src/serve.rs` | Reduce retention or skip serving-only copies when serving is disabled | `--no-serve` already skips the full CL envelope cache, but raw BAL/body/withdrawal retention still occurs; bodies and decoded BALs also support RPC/replay |
| Recent state writes | Reduce `--window`, e.g. 256 to 128 | Roughly halves the ~19.5 MB recent-write component at the same block mix; shortens state-query/replay/reorg coverage |
| Call results in `rpc.rs` | Add a byte budget and age/LRU eviction | The current 20,000-entry bound does not bound variable-size result/revert buffers; eviction increases repeat execution |
| Restart replay in `skim-cli/src/persist.rs::verify_and_replay` | Let `skim-state/src/persist.rs::replay` iterate over borrowed writes | Removes a duplicate of the entire buffered write batch; reduces catch-up peaks rather than steady-state growth |
| Startup BAL seeding in `seed_recent_bals` | Discard reads, touched addresses, and code before retaining the seed batch | Reduces temporary startup allocations; the store already strips these fields from its recent-write index |

Core `Store::gc` already expires on-demand keys, prunes recent blocks, collapses old versions
to one base, and removes unreferenced code. Its eviction is not missing in the audited `run`
path. However, vectors and hash maps retain allocation capacity after entries are removed;
reporting only their lengths understates live allocated memory. Consider conditional shrinking
after large contractions once capacity measurements justify it.

### Missing cleanup and unbounded retention

These are source-level findings. Their contribution to the measured growth has not been
quantified, and some require timeouts, peer churn, unusual requests, or stalled work to occur.

1. **SNAP/1 pending requests survive caller timeout.** In
   [snap1.rs](../../crates/skim-el/src/snap1.rs), `pending` removes a sender only when its response
   arrives or the connection is dropped. `ElNet::snap_any` in
   [lib.rs](../../crates/skim-el/src/lib.rs) drops the receiver on timeout, leaving unanswered
   entries for the session's lifetime. Remove canceled pending requests, skip canceled queued
   commands, and bound the command queue. Cancellation should trigger cleanup even if the peer
   sends no further traffic.
2. **Peer history never evicts.** In [node.rs](../../crates/skim-cl/src/node.rs), `last_dial` and
   `seen_nodes` accumulate distinct peers/nodes. In [book.rs](../../crates/skim-el/src/book.rs),
   `PeerBook.records` retains every historical peer. Expire dial cooldowns and bound or age
   history records while preserving useful ranking and aggregate statistics. Bounding discovery
   deduplication changes lifetime-unique counting semantics and should be reflected in metrics.
3. **Dashboard method names are unbounded.** `Dashboard::record_method` in
   [dashboard.rs](../../crates/skim-cli/src/dashboard.rs) inserts arbitrary request method strings.
   Normalize unknown methods to `other`, as the adjacent metrics path in `Rpc::one` already
   does. Also apply the address limit in `record_hit`: it inserts addresses without the pruning
   used by `record_call`, and state-override calls can reach it directly.
4. **Canceled scheduler jobs can remain queued.** In
   [skim-snap/src/lib.rs](../../crates/skim-snap/src/lib.rs), closed waiters are retained, and a job
   waiting for its first send has no deadline while peers exist. Low-priority capacity becomes
   zero when a peer's adaptive capacity is one. Remove orphaned jobs/closed waiters and add
   queue admission limits and a bounded wait for capacity.

### Recommended order and verification

1. Use the platform-appropriate allocator, based on the existing macOS/Linux comparisons.
2. Fix timeout/cancellation cleanup, bound peer history, and normalize dashboard method names.
   Add focused regression checks for unanswered requests, canceled queued work, peer churn,
   and repeated unknown method names.
3. Expose counts, estimated bytes, and allocation capacities for chain-data caches, call results,
   learned sets, peer history, and pending scheduler/SNAP work in `skim_status`.
4. Re-run the identical-load A/B without restarts. Compare store/cache occupancy and live heap,
   exercise the cache bounds, and test cancellation/churn separately from ordinary head following.
5. If lower steady-state RSS is still wanted, tune cache horizons/byte budgets. Remove the replay
   clone separately to reduce restart peaks. Measure savings and RPC/recovery effects rather than
   adding the estimates above as if they were observed RSS reductions.

## How to reproduce
- **Driver:** `docs/experiments/tools/mem_ab.py`.
  - Runs N builds side by side, each with soak config A (pool + quote subscriptions, Uniswap V3
    predictor) on its own ports and data directory.
  - Per new head it sends the same requests to each build, seeded by block number: 3 random
    quotes, the subscribed quote, 5 pool slots, a BLOCKHASH probe, 3 sender balances, and an
    archived read every 4 blocks.
  - Every 30 s it samples RSS and `skim_status`.
  ```
  python3 -I docs/experiments/tools/mem_ab.py <out-dir> 240 sys=<skim built without mimalloc> mi=target/release/skim
  ```
- **Ports:** instances are spaced 10 apart (skim also binds a neighbouring port). Use `BASE=<n>` to
  run two drivers at once.
- **Build without mimalloc:** in a worktree, delete the two lines `#[global_allocator]` and
  `static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;` in `crates/skim-cli/src/main.rs`, then
  build with `cargo build --release -p skim-cli` into a separate `CARGO_TARGET_DIR`.
- **Heap split on macOS:** `vmmap --summary <pid>` gives the physical footprint and the
  `DefaultMallocZone` line (bytes allocated, system allocator only). With the system allocator,
  `heap <pid>` adds a breakdown by size.
- **Old binaries:** soak data directories (`data/soak4/skim-bin` = `5237bc1`,
  `data/soak5/skim-bin` = `be3f0fa`) are local and gitignored. Rebuild from those commits if they
  aren't there.
- **Caveat:** several instances on one IP compete for EL peers (fresh nodes saw `TooManyPeers`
  for minutes). Memory comparisons are fine; latency comparisons aren't.

## References
- `docs/experiments/m7-gates-and-acquisition.md`, section "Memory: why head-only RSS went from
  ~134 MB to ~230 MB", and the soak results (third soak).
- `docs/TODO.md`: item 6 was closed in `47ceff4` with the reasons above. It has since been
  reopened as "whether RSS levels off" ([TODO 6](../TODO.md#memory)).
- Earlier catch-up notes: heaptrack on aarch64 Linux showed a 228 MB peak live heap (freed receipt
  buffers). It couldn't unwind the release binary, so a symbolized profile still needs a build
  with debug info (`[profile.release] debug = "line-tables-only"`).
