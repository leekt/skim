# M7: regression gates, a stricter soak, and peer acquisition

Runs: 2026-10-17 (KST), Sepolia.

Order of work, as agreed after the fourth review:
1. close the known correctness findings with deterministic regression tests, in CI
2. a soak that exercises the dangerous transitions and gates on availability too
3. first-use peer acquisition, with consensus catch-up diagnostics alongside
4. diagnostics, alerts and release packaging
5. memory and throughput

## 1. Correctness findings and CI
The fourth review's findings (storage-mode transitions, `safe`/`finalized` resolution) are fixed with a regression test each; see [M6](m6-correctness-and-bounds.md#fourth-review-storage-mode-transitions). The store tests fail on the previous code.

**CI** (`.github/workflows/ci.yml`) runs on every push and pull request:
- **tests:** the workspace test suite, which includes every review regression
- **fuzz:** a replay of past fuzz crashes

Clippy runs in report-only mode, because older warnings remain. Every push since has passed.

## 2. Soak v2: transitions and availability
`docs/experiments/tools/chaos_soak.py` compares skim with a reference node every block, as before. It now also creates the transitions the reviews found dangerous:

| Transition | How the soak creates it | Probe |
|---|---|---|
| BAL → archive | at each restart, the config gains an archived subscription for an account whose balance, at that moment, exists only in recent BALs | `promo_archived`: that balance at blocks older than the in-memory window (from disk) |
| TTL → whole storage | config B puts the pool under `storage: all` while the restart snapshot hands its keys back as provisional TTL entries, and random pool slots are read on demand from the first block | `promo_slot`: those slots, every block, long after the TTLs |
| BAL-first reads | – | `bal_first`: balances of the current block's senders |
| Fresh fetch vs restore | every restart (the soak keeps a `--data-dir`) | all probes |

Reorgs can't be triggered on a live network; they're covered by the deterministic store tests.

**Gates.** A run fails on any of:
- a wrong answer
- subscribed keys over the limit, or RSS over its limit
- a steady-state answer rate below 95% for subscribed probes or 80% for cold ones (so a client that refuses everything fails)
- a probe kind that never matched
- subscriptions not ready again within 180 s of any restart

**Smoke run (15 min): PASS.**
- **Answers:** 665 matches, 3 refusals, 0 wrong. Every steady-state answer rate was 100%.
- **Recovery:** 142 s (cold start, no snapshot) and 91 s.

That run found two problems, fixed below: a 3-block restart gap downloaded 43 MB, and the first post-restart subscription pass waited for SNAP peers.

**Full run (2 h):** see [Results](#results).

## 3. Peer acquisition
**Measured** over 7 Sepolia startups, from skim's session log:

| Startup | EL sessions | Crawlers | reth (no SNAP) | SNAP-capable | First SNAP peer | SNAP peers that dropped us ("too many peers") |
|---|---|---|---|---|---|---|
| cold | 28 | 16 | 6 | 6 | 61 s | 3 |
| cold | 59 | 36 | 13 | 10 | 45 s | 5 |
| cold | 48 | 32 | 10 | 6 | 47 s | 3 |
| saved peers | 14 | 7 | 3 | 4 | 15 s | 3 |
| saved peers | 16 | 11 | 4 | 1 | 152 s | 0 |
| saved peers | 60 | 30 | 20 | 10 | 6 s | 3 |
| saved peers | 17 | 8 | 6 | 3 | 6 s | 1 |

What the measurements say:
- About half the sessions are crawlers, and reth peers don't offer SNAP.
- About half the SNAP-capable peers end the session within a minute because they're full.
- Saved peers usually get a state peer in seconds, but not always.

So more dialing isn't the lever. Knowing which peers stay is.

**Changes:**
- **`PeerBook`:** records for each peer its client, SNAP support, longest session, and how often it dropped us for being full.
- **Ranked saved list:** peers that held a SNAP session rank first. Crawlers and peers that only ever dropped us are left off the list, even if an older list had them.
- **Metrics and status:** sessions by client and SNAP support, session ends by reason, time to the first SNAP peer, and SNAP peers seen and held (≥ 60 s). Shown in `skim_status.peer_acquisition` and in a Grafana row.

**After a restart**, subscription passes now log their duration. Measured: the first pass started with the first block and no SNAP peer, and took 40 s: 25 s waiting for the first SNAP peer, then ~15 s of fetching. Restart readiness is bounded by state-peer acquisition, not by the subscription logic.

### Consensus catch-up
Every restart's consensus catch-up had timed out. Instrumented first (see [M6](m6-correctness-and-bounds.md#restart-catch-up-via-consensus-peers-measured-then-fixed)):
- **Cause:** peers served every request, but sequential 2–6 s requests used up the 60 s budget.
- **Parallelism:** 8 requests in flight → a 655-block gap in 8.4–9.0 s.
- **Sizing:** the walk now starts only as many windows as the gap needs. A 3-block gap had downloaded 43 MB (262 envelopes). A 48-block gap now takes 61 envelopes, 8.9 MB, 2.6 s.

## 4. Diagnostics, alerts, packaging
- **Alerts:** four Grafana rules, provisioned with the stack: no SNAP peers, head behind, subscription not ready, refusals. They were checked to load and evaluate, and "head behind" fired when the node stopped reporting.
- **Container image:** a `Dockerfile` with a slim runtime that runs as a non-root user.
- **Release workflow:** a version tag builds Linux x86-64 and arm64 and macOS arm64 binaries and publishes the image to GHCR. A manual dry run built all three binaries and the image.

## 5. Memory
Each block in the recent-BAL index holds, on Sepolia:

| Part | Entries / bytes per block | Approx. memory per block |
|---|---|---|
| writes | 727 | ~70 KB |
| read-only slots | 776 | ~43 KB |
| touched addresses | 366 | ~7 KB |
| deployed bytecode | 4.5 KB | 4.5 KB |

Only the writes are read again after a block is applied, so the index now keeps only those: about 44% fewer entries, ~14 MB less with the default 256-block window. This estimate comes from entry counts and in-memory sizes, not a measured RSS difference. `skim_status.store.recent_index` shows the breakdown.

## Throughput
`skim bench-node` with `--no-call-cache` (every call executes), Uniswap V3 config, same machine and node, 5–6 sweeps per build, medians:

| In flight | thin LTO (before) | full LTO | Change |
|---|---|---|---|
| 1 | 1.9k calls/s | 2.4k | +24% |
| 10 | 14.8k | 19.0k | +28% |
| 100 | 16.9k | 21.0k | +24% |

The interpreter dominates CPU, so cross-crate inlining into revm pays off. `release` is now full LTO; CI tests use a thin-LTO `ci` profile to stay quick. Single sweeps vary by about ±20% (different blocks and calls), so the table compares medians. For reference, Nethermind measured 16.8k at 100 in flight in [M5](m5-predictors-and-chain-data.md#5-throughput-under-concurrency), on the same machine but on a different day.

## Fifth review: archive continuity, scan scope, commit races, committee rollover
The invariant behind the first three: **archive coverage belongs to one verified chain and one fixed key set**, not to a run of block numbers.

| Finding | Fix | Regression test |
|---|---|---|
| P1: after an offline reorg (A100 → B100), the first block after a restart (B101) extended the archived segment by number, so K at B101 could inherit A100's value | `write_block` extends a segment only if the block's parent is the archived block at the segment's end; otherwise a new segment starts and nothing carries over. The healer reconciles such boundaries: it walks the current chain back with EL headers to the newest block the archive agrees with, removes the orphaned tail (`truncate_range`), and the gap fill brings the current chain's blocks. A gap whose fetched chain doesn't link is reconciled the same way | `a_restart_after_an_offline_reorg_never_inherits_the_old_fork` (fails with the fix removed): refused, then 20 after reconciliation, never 10 |
| P1: a key archived while a backfill scan was running was evaluated against blocks whose writes the scan had filtered out, and could be extended back as "unchanged" | Scans freeze the archived key set when they start. Backfill skips keys outside that set (a later pass picks them up). A gap fill aborts and retries if a key outside its set is covered from before the end of the gap | `a_key_archived_during_a_backfill_scan_is_not_extended_by_it` (fails with the fix removed) |
| P1: backfill and gap fill validated segments and hashes, then opened the write transaction, so a rollback and fork switch could land in between and leave old-fork values under new hashes | All validation runs inside the committing write transaction. redb serializes writers, and rollback truncation is one too: it lands either before (validation fails) or after (it truncates what was written, through the journal) | `a_fork_switch_during_a_backfill_never_leaves_old_fork_values_under_new_hashes` (a hook switches forks between the checks and the commit) |
| P2: the light client held one committee, picked it with `(signature_slot - 1) / period`, and could only roll forward through a finalized header that the next committee itself had to sign | `Committees` (current + next). The committee is chosen by `signature_slot / period`. The next committee is learned from a `LightClientUpdate` (`LightClientUpdatesByRange`) signed by a held committee, with its branch proven against the attested state (gindex `current + 1`), and only if the attested header is in the current period. Finality in the next period rotates it to current | `the_committee_rolls_into_the_next_period` (bootstrap late in P, authenticate P+1, the last slot of P and the first slot of P+1, rotation; fails with the old period rule), `a_next_committee_must_be_proven_and_belong_to_the_current_period` |

The light-client tests use generated fixtures (BLS keys, Merkle branches), so they need no Sepolia light-client data.

## Sixth review: overlapping restart heights, soak accounting, multi-period catch-up
| Finding | Fix | Regression |
|---|---|---|
| P1: when the first block after a restart was at a height the archive already held (B100 over A100), `write_block` returned `NotNext`, the store only counted the error, and a later verified base at B100 was written to the archive by block number, under A100's header | At a height the archive already holds, the same block is already archived (continuity kept). A different block means the archive holds an abandoned fork from there up, which is dropped before writing. `write_base` takes the block hash (and each later write's hash) and checks them against the archive inside the committing transaction | `a_restart_at_an_archived_height_never_stores_the_new_fork_under_the_old_hash` (through `Store`): fails with both protections removed. The hash check alone prevents the corruption; the overlap handling also keeps B100 archived. `a_restart_at_the_same_archived_block_keeps_continuity` |
| P2: the soak skipped requests that skim didn't answer (timeouts, connection failures), so 1 success + 99 timeouts read as 100%; stalled heads skipped probes; "every kind matched" checked only kinds that appeared | Transport failures count as unanswered. Wall-clock freshness: at most 2% of steady-state time with a head older than 3 slots. An explicit set of required probe kinds, each with a minimum sample count | harness selftest; smoke run |
| P2: the soak's "BAL → archive" probe didn't establish that the transition happened, and sampled blocks before coverage could start | Renamed `restart_archive` (restart plus archive). It reads only from where skim reports the key covered (new `skim_archiveCoverage` RPC), so a refusal inside coverage is a failure. `skim_status.store.bal_promotions` counts promotions from recent BALs; the soak reports how many happened. BAL → archive promotion itself is covered by the deterministic tests | – |
| P2: a checkpoint two or more periods old stalled: catch-up fetched only the next committee, and nothing applied historical finality | Catch-up is a sequential full-update loop (`light::apply_updates`). Each update is verified in order; it teaches the next committee, applies its finalized header (rotating the period when finality reaches the next one), and is re-checked after a rotation for the new next committee. The node repeats `LightClientUpdatesByRange` until the held period is the live one | `a_client_two_periods_behind_catches_up_from_full_updates` (bootstrap in P, updates for P, P+1, P+2, then live P+2 gossip verifies; the state roots commit to both the next committee and the finalized header), `catch_up_stops_at_the_first_update_it_cannot_verify` |

## Seventh review: block hashes, aged bases, catch-up publishing, head age
The invariant from the fifth review, extended to every EVM input: **a known value or hash is usable only if it belongs to the block being simulated.**

| Finding | Fix | Regression test |
|---|---|---|
| P1: `BLOCKHASH` fell back to whatever hash the archive held at that height. After an offline reorg (A99 → A100 on disk, restart at B101, reconciliation pending), `BLOCKHASH(99)` at B101 returned A99 | `Store::block_hash_at(at, n)` replaces the global lookup in every simulation path (calls, `eth_simulateV1`, access lists, traces). In-memory hashes are used when `at` is on the in-memory chain. An archived hash is used only if the archive puts it in one segment with a block known to be an ancestor of `at`: the nearest in-memory hash above `n`, or `at` itself for an archived snapshot. Segments are parent-linked, and the check runs in one read transaction. Anything else is a miss, so linked headers are fetched, or the call is refused | `blockhash_never_returns_an_archived_hash_from_an_abandoned_fork` (`None`, never A99; B99 once linked headers establish it) and `blockhash_uses_archived_ancestors_that_link_to_the_simulated_block`. Both fail with the archive check removed |
| P2: promoting a long-unchanged key to the archive dropped its only version (its block's hash had left the ring), registered the key with no base, and marked it archived | A version whose block hash is gone moves up to the lowest later block whose hash is known in memory and archived; it is still the key's value there. Coverage starts there: never earlier, never without a base | `archiving_a_long_unchanged_key_keeps_its_value_from_a_known_block` (K = 10 from block 1, promoted at 400: archive reads at 400 and 401 return 10, block 1 is not covered). Fails with the old filter |
| P2: catch-up verified attested and finalized headers but published only the period; `safe`/`finalized` waited for gossip | One `publish` path for gossip and catch-up. Attested and finalized execution hashes move forward only (by beacon slot); counters and the persisted finalized root update; the next block resolves them | `catch_up_from_range_updates_publishes_safe_and_finalized_without_gossip`: range updates only, no gossip. Status reaches P+2, `verified_block` and `finalized_block` resolve to the blocks the headers commit to, and an older header doesn't move them back. The fixtures now live in `skim_cl::light::testkit` (feature `testkit`) |
| P2: soak freshness measured arrival gaps, so a stream of old blocks arriving regularly passed. The timer also shared the probe loop | A separate thread samples every 2 s. A head is stale if its header timestamp is more than 3 slots old, or it is more than 3 blocks behind the reference head. The longest gap between heads is reported separately (`max_head_gap_s`) | `--selftest`: steady, regularly arriving 5-minute-old headers fail the gate (fails if the age check is removed) |

## Eighth review: pending downloads, scheduled downtime, header-only misses
| Finding | Fix | Regression |
|---|---|---|
| P2: a `storage: all` subscription whose download kept failing (unavailable, oversized, over the budget, backing off) was already watched, so every BAL write to the contract was kept, outside every budget | The address isn't watched until the storage is installed; a pending download keeps nothing. Installation replays the writes between the download block and the head from the BAL index. If the index no longer covers them, the download is refused as too old (`OutOfWindow`) and runs again, instead of silently missing writes | `a_failing_whole_storage_download_does_not_accumulate_the_contracts_writes`: 39 blocks write new slots while installs fail; 0 keys and 0 versions retained; after a successful install the replayed writes read correctly. `a_download_older_than_the_replay_window_is_refused`. These test the store side; the removed `watch_address` call is in subscription maintenance, which has no network-free test harness |
| P2: the soak's freshness sampler counted the scheduled downtime between stop and relaunch as steady-state staleness | A shared `Phase`: a disruption starts just before the process is stopped and ends at recovery; recovery is still timed from the relaunch. The probe loop and the sampler read the same phase | `--selftest`: scheduled downtime adds no stale time, and an unexpected outage after recovery does (fails with the exclusion removed) |
| P2: a run whose only miss was an ancestor header still waited for a SNAP peer (headers come from any eth peer), and could be refused with `NoStatePeers` | Header-only misses resolve the headers and run again, without the SNAP wait (calls, `eth_simulateV1`, traces). Header-only retries count toward the round limit | No end-to-end test (it needs a peer that serves headers but not SNAP); `Access::has_state_misses` separates the two |

## Ninth review: post-fork history must stay on one chain
The history added after the soak (`eth_getLogs` over post-fork headers on disk) repeated the archive's old mistake: trusting rows by height.

| Finding | Fix | Regression test |
|---|---|---|
| P1: the header store linked runs by consecutive height, so after an offline reorg (A98-A100 on disk, restart at B101), a query for 99 after one for 100 read A99 as B100's parent: logs from the abandoned fork, or a wrong empty result | Nothing on disk is trusted by height. Reads walk down by hash from the oldest in-memory (canonical) header, in one read transaction (`linked_range`), and refuse if the chain breaks. The header walk jumps over a stored run only as far as it is hash-linked (`linked_low`); a mismatch is fetched again | `history_after_an_offline_reorg_never_reads_the_old_fork`: 100 then 99, refused until B99 is fetched, then 98-100 are A98, B99, B100. It fails with height-only linking |
| P1: `eth_getLogs({blockHash})` resolved the hash to a height, and the log scan then used whatever block was at that height | The requested hash goes through the scan; a different block at that height is refused (`-32002`). The WebSocket `logs` subscription pins each block the same way | none yet (needs a reorg between resolution and the scan) |
| P2: a downward repair batch (newest first) deleted the headers it had just inserted, because each replacement truncated everything above it | Two writes: `put_tip` (gossip; a replaced height drops what's above it) and `put_run` (a fetched run; overwrites its heights, nothing else). Reads check links, so leftover rows are harmless | `a_descending_repair_batch_keeps_every_header_it_brings` |

The 150-minute soak predates this history path; it doesn't validate it.

## Tenth review: stream cursors, local logs, cancellation
| Finding | Fix | Regression test |
|---|---|---|
| P1: smallname applied a stream response in one transaction, but a `rewind` called `rollback`, which opened another; SQLite doesn't nest them, so every rewind failed and the module retried the same cursor forever | `rollbackInTx` runs inside the response's transaction: the rewind, the new fork's events and the cursor commit together, or not at all. The apply step is `applyResponse`, testable without a node | `examples/smallname/test.mjs` (in CI): A100 stored, then `rewind 99` + B100: only B100's event and B100 as cursor remain; a response that fails halfway leaves everything as it was. Fails with the nested `rollback` ("cannot start a transaction within a transaction") |
| P1: a stream request waiting at the head checked the cursor once, before waiting; a reorg at the same height during the wait returned no events and the new fork's hash as the cursor, so the module adopted it without undoing the old block | After waiting, the module's position is checked again: a cursor no longer canonical rewinds then and there. An empty answer returns the module's own cursor, never another block's hash at that height. A scan's first block must have the module's position as its parent (by hash), or the request is refused (retry) | `a_reorg_at_the_same_height_during_a_head_wait_rewinds_instead_of_moving_the_cursor`: rewind to 104, then B105 (fails without the re-check). `an_empty_answer_keeps_the_modules_own_cursor` documents the empty answer; it doesn't discriminate (without a reorg both hashes agree, and with one the re-check fires first) |
| P2: matches stored locally were refused once peers expired those blocks (the peer check ran before the store was read), and the stream turned the whole range into a gap | The peer-retention check applies only to blocks that still need fetching, after stored matches are taken. Those blocks become per-run `gap` events between the applied blocks; the leading stream gap now means only "no headers" (before the fork, or head-only outside the window) | `stored_matches_are_served_after_peers_expire_them`: block 118-122 served from disk with every peer's earliest at 200; without the store, one gap 118-122 |
| P2: a scan reserved blocks for fetching, then waited for blocks another scan was fetching, and only then created the guard that releases its reservations: cancelled while waiting, they stayed forever (later scans waited the 60 s timeout) | The guard owns each reservation as it's made, before any await | `a_scan_cancelled_while_waiting_releases_its_own_reservations` (fails with the late guard) |
| P2: the CPU pool's permit was held by the async waiter, so cancelling it freed the slot while the blocking work continued | The permit moves into the blocking task and is released when the work ends | `a_cancelled_wait_keeps_its_cpu_slot_until_the_work_ends`: cap 1, first job cancelled while running, second doesn't start until the first ends (fails with the old permit placement) |

Also from the review: stored matches are encoded and compressed before the write transaction opens; the transaction only stores finished bytes.

## Results
**Full soak (150 min, binary `c020e7d`, soak v2 harness before the seventh-review changes): FAIL on two gates, with no wrong answers.**
- **Answers:** 7,235 matches, 0 wrong, 0 refusals. 43 answers were unavailable from the reference, and 22 skim requests failed in transport (counted as unanswered).
- **Steady-state answer rate:** 99.2% or higher for every probe kind. Every required kind exceeded its minimum sample count (`restart_archive`: 264 samples).
- **Promotion:** 18 keys moved from recent BALs to the archive during the run.
- **Memory:** RSS stayed under 158 MB. The subscribed-key limit was never exceeded.
- **Restarts:** 16 in total.
  - 14 were ready again within 22–148 s.
  - 2 missed the 180 s gate: 407 s and 215 s. Both were config switches, where the new config subscribes keys the snapshot doesn't hold, so readiness waits for SNAP fetches.
- **Freshness:** the head was stale for 4.1% of steady-state time, over the 2% gate. This run measured gaps between head arrivals, the metric the seventh review replaced, so the figure includes the time the probe loop itself was busy.
- **Next:** rerun with the new freshness sampler. If config-switch recovery still misses 180 s, time the SNAP fetch to see whether it is waiting for peers or fetching.

**Second full soak (150 min, binary `5237bc1`, harness with the head-age freshness sampler and scheduled-downtime exclusion): FAIL on recovery only, no wrong answers.**
- **Answers:** 7,426 matches, 0 wrong, 0 refusals. 8 were unavailable from the reference; 23 skim requests failed in transport (counted).
- **Steady-state answer rate:** 99.8% or higher for every kind; all required kinds sampled.
- **Freshness:** head stale 0.0% of steady-state time (gate passed); longest gap between new heads 35.6 s.
- **Memory:** RSS at most 134 MB. Subscribed keys never over the limit. 52 BAL promotions.
- **Restarts:** 16; 14 recovered in 34-160 s.
  - **428 s (sigkill, config B):** after 133 s of following, one gossip payload was missed. The gap could only be filled from eth/71 EL peers, and only crawlers were connected on the EL side for 5 minutes. Fixed after this build: a live gap now falls back to payload envelopes from consensus peers (commit `0ce59c6`).
  - **245 s (graceful, config A):** the chain was followed from 33 s on, but no SNAP-serving peer was connected to fill the subscriptions until ~245 s. This is peer acquisition (section 3), not the store; the soak ran next to two other skim nodes on the same machine and IP.
