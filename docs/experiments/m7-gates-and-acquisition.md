# M7: regression gates, a stricter soak, and peer acquisition

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs and follow-ups: 2026-10-17–19 (KST), Sepolia.

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

## Eleventh review: one chain per stream response, atomic coverage, seeded archive keys
| Finding | Fix | Regression test |
|---|---|---|
| P1: a combined stream scanned logs and BAL changes separately, each resolving its own headers, and merged them by block number. A reorg between the two could return A101's block and logs with B101's state changes. BAL-only streams never checked the cursor's parent, and the cursor was looked up again at the end | One hash-linked header range (`linked_headers`) is captured per response, checked to continue the module's cursor by hash (BAL-only too), and used for logs (`scan_headers`), BAL changes and the new cursor. Nothing is resolved again. BAL entries whose hash isn't the range's are refused | `logs_state_changes_and_cursor_come_from_one_chain_despite_a_reorg_mid_request` (reorg after the log scan: all-A response; fails if BALs re-resolve the range). `a_bal_only_stream_never_continues_a_cursor_onto_another_fork` (reorg before capture: refused, then the retry rewinds; fails without the parent check) |
| P2: an expired-receipts `gap` at a block took that block's BAL changes into the gap event, so a consumer skipped them | Logs and state changes are separate channels. Gaps carry `scope` (`logs`, or `all` for no headers). A block in a logs gap still gets an `apply` with its `stateChanges` and `logsAvailable: false` | `state_changes_still_arrive_for_a_block_whose_receipts_expired` |
| P2: a scan read a block's stored coverage before reserving it, and `put_block_logs` replaced the entry, so two scans could overwrite each other's coverage and logs | `put_block_logs` merges with the stored entry inside its write transaction (union of coverage, and logs by position). The common case still stores bytes compressed beforehand | `a_scan_storing_from_a_stale_snapshot_never_drops_what_another_stored_since` |
| P2: startup BAL seeding made an archived key known in memory without writing it to the archive, and later registration returned early, so its history stayed uncovered | Keys promoted by seeding that are archived get their versions written through the hash-checked base path. A value older than the archive starts at its first block, never earlier | `an_archived_key_seeded_from_earlier_bals_gets_archive_coverage`: readable at 20 and 30 after the window moved on, not at 17 |
| Earlier finding, partly open: `eth_getLogs` scans with `persist = false`, which also stopped it reading stored matches | Reading and storing are separate: stored entries are read whenever every atom of the filters is known; only streams store | `eth_get_logs_answers_from_stored_matches_after_peers_expire_them` (and a filter never stored for still refuses) |

Every fix above was mutation-checked: reverting it fails its test.

## Twelfth review: smallname's discovery boundary and inventory, reloaded archive keys, metric labels
| Finding | Fix | Regression test |
|---|---|---|
| P1 (smallname): a batch that discovered new contracts committed its events but kept the old cursor (to re-read with wider filters). A reorg of those blocks before the re-read left them indexed: the cursor was still canonical, so no rewind came | Such a pass is provisional: the whole transaction rolls back, the found contracts are kept in memory for the next request's filters, and the batch commits only with its cursor | `examples/smallname/test.mjs`: "a pass that finds new contracts stores nothing, so a fork can't leave its events behind" (A101 announces a resolver, B101 replaces it: no orphan event or resolver; fails with the old commit-then-rescan) |
| P2 (smallname): `record_keys` kept one row per key and overwrote its position, so rolling back the latest write deleted the key | `record_key_writes` keeps every write; inventory is "keys written after the latest version reset". Old index files migrate on open | "a reorg restores a record key's earlier write…" (compared with an index of the surviving chain only), and the migration test |
| P2: a key reloaded as archived after a restart, then promoted from seeded BALs when a subscription asked for it again, returned early ("already archived") without archiving the value | The early return still writes newly promoted versions as the segment's base | `a_reloaded_archived_key_promoted_after_seeding_gets_a_base_in_the_new_segment` (reverse ordering, no healer) |
| P2: `known_method` checked only a name's shape, so every made-up `eth_*` name got its own dashboard entry and metric series | The label comes from dispatch: "method not found" is counted as `other` | `unknown_method_names_share_one_label_however_many_there_are` (10,000 names, one entry) |

Every fix above was mutation-checked.

## Thirteenth and fourteenth reviews: live-read failures, registry placement, archive warm-up, the transaction index
The thirteenth review's three findings (smallname P1 and P2, archive P2) were fixed together with the fourteenth review (of `9ed1f49`, the pinned transaction index).

| Finding | Fix | Regression test |
|---|---|---|
| P1 (smallname): failed live reads were folded into null/absent. An unanswered `getSubregistry` ended the ENSv2 walk and fell back to the ENSv1 answer (wrong authority); unanswered `nameExpires`, `ownerOf` or old-registry `owner` gave a profile without an expiry or owner | A failure that isn't a revert (code 3) makes the name's state unavailable (`read_status: failed`); a revert, such as `ownerOf` of an expired lease, still means absent | `examples/smallname/test.mjs`: "a read skim couldn't answer is unavailable, never an absent value; a revert still is absent" (fake reader) |
| P2 (smallname): `mount()` overwrote a registry's place and its tokens' nodes in place, keeping the rows' old `block`; rollback only deletes newer rows, so a re-mount from an abandoned fork stayed | A mount records what it overwrote (`mount_undo`, under its block); rollback replays those newest-first before deleting | "a reorg puts a re-mounted registry and its tokens back where they were" (re-mount at 105, rollback to 102 and 95) |
| P2 (archive): an archived key reloaded after a restart and warmed by an on-demand read (`touch_ttl`) had its value seeded into memory only; the subscription asking for it later found nothing to seed and wrote no base | `touch_ttl` archives an archived key's value when it seeds it (as the BAL seeding does); a resubscription writes whatever is known as the base, whoever seeded it | `a_reloaded_archived_key_warmed_by_an_on_demand_read_first_still_gets_a_base` |
| P2 (tx index): looking up an indexed transaction whose body and BAL had left their caches refetched the body and re-indexed the block from "no BAL", erasing its pinned entries (first lookup fine, later ones null) | A refetch leaves a block indexed under the same hash alone; pinned mode without the BAL decides nothing | `refetching_an_evicted_body_keeps_the_blocks_pinned_entries` |
| P2 (tx index): included sent transactions were never dropped (two maps kept each other alive) and, once the header window dropped their block, were answered as pending | pending -> included (with the block's header and hash) -> expired after 24 h or beyond 4,096; a reorg, or another block noted at that height, makes them pending and re-broadcast again | `sent_transactions_move_pending_included_and_expire_with_a_bound` (index and header windows passed, body evicted, reorg, count bound) |

All mutation-checked. Two fixes have a second guard that no test isolates: the "same hash already indexed" check behind "no BAL decides nothing", and the resubscription base write behind the warm-up archive write. Each test fails with both halves removed.

## Fifteenth review: ingestion order of the transaction index, archive rewrites, smallname lists
| Finding | Fix | Regression test |
|---|---|---|
| P1: `skim run` notes a payload before the chain placer decides on its block, and the payload marked sent transactions included right away. A block never adopted (placement deferred: parent missing) made a transaction "included" and stopped its re-broadcast; in a same-height reorg the new fork's inclusion was recorded first and then wiped by the old fork's cleanup | A payload only fills caches. Indexing and inclusion happen once the block is canonical (header noted) and its body cached, whichever comes second, after a reorg's cleanup. The test helper follows `skim run`'s order | `a_payload_skim_hasnt_adopted_never_makes_a_transaction_included`, `a_same_height_reorg_includes_the_new_forks_transaction` (both fail with the old order) |
| P2: the archive warm-up fix made every registration of an archived key rewrite its base, and subscription maintenance registers archive keys every block: one redb commit per key per block, under the store's write lock | The store records which archived keys have their base in the current segment (and at which block); a registration writes only when it's missing. Cleared on release and, above the fork point, on rollback. `StoreSizes.archive_base_writes` counts writes | `re_subscribing_an_archived_key_whose_base_is_written_writes_nothing` (50 registrations, no extra writes) |
| P2 (smallname): list routes treated a name whose live read failed as a non-match and moved the cursor past it, so a page walk could silently miss a name | A check returns a row, null (a confirmed non-match) or `UNAVAILABLE`; the page stops before an unavailable candidate (`meta.incomplete`, cursor resumes there), or fails with 503 if it's the first | "a list never skips a name whose live read failed…" (`fill` over a, b, c with one read failing) |

All mutation-checked. Not done: batching the initial base writes of many keys into one commit (each key is now written once per segment).

## Sixteenth review: chain data follows the store; the archive base cache checked
| Finding | Fix | Regression test |
|---|---|---|
| P2: `chain.rs` noted a header in ChainData (canonical index, transaction inclusion) before the store applied the block, so a block the store refused could be canonical for transaction lookups while the store's head stayed on its parent | `Chain` has two callbacks: `on_header` (header caches execution needs, before the apply) and `on_applied` (ChainData, only after the store applied the block) | `a_block_the_store_refuses_is_never_reported_as_applied` (a child of the head by hash at the wrong height: `Deferred`, nothing reported; the real child: reported) |
| P2 (to verify): does the `based` cache (archived keys whose base is written) agree with archive coverage across segment ends and reorgs? | Checked: coverage ends only through `release_archived`, which clears the entry; `truncate_above` drops exactly the coverage starting above the fork point, as `based` drops entries above it; a restart starts empty. No change needed | `the_based_cache_follows_coverage_through_release_and_reorgs`: after each transition, every based key's coverage starts at or before its base (fails without either invalidation) |

Metrics the review asked for: `skim_store_archive_base_writes_total` (expected ~0 per block once archived keys are based) and `skim_subscription_maintenance_seconds` (P50/P99 in Grafana, "State and addresses").

## Seventeenth review: reorgs and chain data
| Finding | Fix | Regression test |
|---|---|---|
| P2 candidate: on a reorg the store rolled back, then the new block was applied, while ChainData kept the old fork's headers and index until `on_applied`; a request in between could see the store rolled back and the old fork canonical in chain data | Every rollback goes through `Chain::rollback`: `on_rollback` first (`ChainData::rollback_to`: headers, index entries and inclusions above the fork point dropped; sent transactions there pending again), then the store. A request in the gap finds nothing at those heights (`-32002`) instead of the old fork | `a_reorg_rolls_chain_data_back_before_the_store_then_applies_the_new_block` (fails without the callback), `rolling_back_forgets_the_old_forks_blocks_and_inclusions` |

Also from this review, tracked in [TODO](../TODO.md): the hedged-fetch A/B (cold-read p95/p99 and the extra requests per peer), a long zero-config soak, and signed release artifacts or provenance (the install script's SHA-256 check catches corrupted downloads, not a compromised release).

## Eighteenth review: requests across a reorg
| Finding | Fix | Regression test |
|---|---|---|
| P2 candidate: `on_rollback`, the store's rollback and the new block's apply aren't one atomic step, so a request running across them could read the old fork in one place and the new one in another | A reorg epoch in the store, bumped before anything is rolled back. Requests capture it with `latest` (once per batch); a request whose epoch moved gets `-32002 chain_changed` (retry) instead of its result. Exempt: methods that don't read the chain, `skim_streamNext` (its cursor rewinds) and `eth_sendRawTransaction` (already broadcast) | `a_request_spanning_a_reorg_is_refused_never_answered`; the reorg-order test checks the epoch moved before `on_rollback` (fails without the early bump) |

The review's two measurement items: hedging under injected faults (below, "Hedged fetch") and the zero-config long run (TODO 6, a 30-hour soak running).

## Peer requests: coalescing and fetch order
Measured on the aarch64 Linux deployment host (2 cores), each run on a fresh instance with its own ports and data directory, counted with `skim_status.peer_requests`.

**Coalescing (`--history local`, the deployed build, `466f36e` or later).** After a warm-up stream walked the headers, N identical `skim_streamNext` requests ran concurrently over the same 512 blocks (the ENS registry and WETH as filters, ~45 matching logs). Without coalescing, each stream would fetch the candidate blocks' receipts itself, so N streams would cost about N times one stream's requests. The receipts requests (`GetReceipts`, 32 blocks each) for the whole set of streams were:

| Concurrent streams | Runs | `GetReceipts` requests | Wall time |
|---|---|---|---|
| 1 | 2 | 5, 6 | 6.0 s, 46.1 s |
| 2 | 2 | 5, 6 | 4.4 s, 3.7 s |
| 3 | 4 | 6, 6, 6, 7 | 3.5 s, 3.2 s, 3.6 s, 43.9 s |

Every concurrent stream got the same events. So a second or third reader of the same range adds no receipts requests: the coalescing works. 7 of 15 runs failed in the warm-up instead and are left out. A fresh instance's walk back to 2,600 blocks below the head stopped at about 600 with `-32002 headers below N not served by peers` (the four peers it asked didn't return a linked run), and 30 s of retries weren't enough. See [TODO](../TODO.md).

**Fetch order (binary `64aa3ee`, `SKIM_FETCH_PIPELINE=1` against `=0`).** The workload was 40 cold `eth_call`s through the Universal Resolver, `resolve(name, addr(node))` for 40 `.eth` names from the smallname index, sent as one batch 30 s after the instance was ready. Alternate runs used each mode:
- `1`: each account's bytecode is requested as soon as that account arrives.
- `0`: the earlier order, where bytecode is requested in one batch after the whole wave.

| Mode | Runs with 40/40 answered | Cold batch time | SNAP account / storage / code requests |
|---|---|---|---|
| completion order (`1`) | 4 | 51.6, 52.6, 20.8, 48.7 s | 401\*, 66, 64, 70 / 459, 384, 384, 396 / 74, 49, 50, 53 |
| wave order (`0`) | 3 | 14.9, 6.1, 50.0 s | 76, 42, 49 / 390, 362, 367 / 43, 41, 46 |

\* The first run, unexplained; the other runs agree with each other.

Four more runs (two of each mode) lost every SNAP peer partway through ("no SNAP state peers connected after waiting 60 s"). They answered 32 of 40 calls in 75–105 s and are left out. The same batch was warm in 34–58 ms in every run.

**Conclusion:** completion order shows no latency gain on this workload. Time is dominated by which SNAP peers a fresh instance has (6–53 s with all calls answered). It does cost about 15% more code requests, because codes go one per request instead of batched. The switch was for this measurement only and has been removed; completion order stays. Batching codes that arrive close together is the obvious refinement, if code requests start to matter.

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

**Third full soak (150 min, binary `be3f0fa`, head-only, the same harness): FAIL on one recovery, with no wrong answers.**
- **Answers:** 8,873 matches, 0 wrong, 0 refusals. 17 answers were unavailable from the reference; 10 skim requests failed in transport (counted).
- **Steady-state answer rate:** 99.7% or higher for every kind, and every required kind was sampled.
- **Freshness:** the head was stale 0.0% of steady-state time; the longest gap between new heads was 24.9 s.
- **Restarts:** 17; 16 recovered in 19-151 s.
  - **235.5 s (sigkill, config A):** the chain was followed again 15 s after the relaunch, and the restart snapshot was verified and replayed 3 s later. The key that wasn't ready was the one the harness subscribes at each restart: the balance of an account that just transacted, which no snapshot can hold. Its value was in a BAL from just before the restart, but after a start the recent-BAL index only covered new blocks, so it waited for SNAP. The only SNAP-capable peer, a Besu node, answered "root unavailable", and the first Nethermind peer connected 226 s in. The same cause explains the second soak's 245 s restart. Fixed in `0e2acfc`: after a start, the 32 blocks before the first applied block are fetched from consensus peers into the index, and keys still waiting are promoted from it (measured live: 31 blocks in 1.7 s).
- **Memory:** RSS median 181 MB, quartiles 157/210 MB, max 263 MB, against at most 134 MB for `5237bc1`. 26 BAL promotions; subscribed keys never over the limit. The reasons are measured below.

## Memory: why head-only RSS went from ~134 MB to ~230 MB
> Later: the [memory audit](memory-audit.md) and, for current long-run measurements, [hardware requirements](../guide/hardware.md#memory) (TODO 6).

Builds side by side on one machine (macOS), each with soak config A and the same per-block requests (seeded by block number), with no restarts. RSS in MB at equal uptime:

| Build | 10 min | 20 min | 30 min | 38 min | 60 min max |
|---|---|---|---|---|---|
| `5237bc1` (system allocator) | 110 | 135 | 149 | 161 | 173 |
| `be3f0fa` without mimalloc | 104 | 130 | 145 | 155 | 172 |
| `be3f0fa` (mimalloc) | 186 | 217 | 243 | 266 | 287 |
| current (mimalloc) | 192-211 | 238-251 | 261 | 273 | 301 |
| current without mimalloc | 129 | 149 | | | |
| current, mimalloc with eager arena commit off and purge delay 0 | 210 | 241 | | | |

All builds held the same state (1,351 keys, 256 recent BALs, ~19.5 MB of BAL writes).
- **mimalloc (`106e3dc`), about 80-110 MB.** The same code with the system allocator stays at the old figures. With the system allocator the live heap was 103 MB at 57 min (malloc zone); with mimalloc the physical footprint was 236-255 MB against 86-94 MB, with the same allocation counts. So it is memory mimalloc holds after freeing it, not more live data. Changing its settings didn't help. It was adopted for catch-up on Linux, where glibc's per-thread arenas left RSS at 394-610 MB.
- **The window filling, the first ~50 minutes.** Every build grows until the 256-block window is full (256 x 12 s = 51 min) and the recent-BAL index and versions reach their size. Soak segments are 7-13 minutes, so soak peaks depend on uptime.
- **Changes since `be3f0fa`, about 20 MB.** The current build without mimalloc is ~20 MB above `be3f0fa` without it at equal uptime (129 vs 104 at 10 min). It also runs more threads (26 vs 15 to 16), which mimalloc gives per-thread pages.
- **Leaks:** none seen within 60 minutes. Growth follows the window filling, and keys and BAL sizes stayed equal across builds. A run longer than the window would be needed to rule one out.

## Header walks right after start
Header and body requests went only to eth/70+ peers (the filter receipts need), at most 4 per pass over ~1.5 s. On a fresh instance there were often none, so a stream below the memory window failed at once with `-32002 headers below N not served by peers`. Locally, 3 of 5 fresh starts failed and no peer was even asked. Fixed in `0e2acfc`: headers and bodies go to any non-crawler peer, up to 8 per pass, waiting up to 30 s for a first one.

Before/after on the deployment host: a `--history local` instance, a stream 2,600 blocks below the head 30 s after it was ready, retried every 10 s for 3 minutes, alternating builds. Seconds from launch:

| | Fresh starts | Restarts (saved peers) |
|---|---|---|
| deployed (`466f36e`+) | 93, 61, **failed**, 161 | 171, **failed**, 176 (up to 11 tries) |
| `0e2acfc` | 125, 169, 98, 64 | 60, 77, 271 (1-3 tries) |

All 7 succeeded with the fix. When no EL peer serves headers at all, which can last minutes for a fresh node (locally every reth peer refused with `TooManyPeers` for 6 minutes), nothing can serve the walk; it now waits instead of failing at once.

**Fourth full soak (150 min, binary `0e2acfc`, head-only): FAIL on recovery and the key limit, no wrong answers.**
- **Answers:** 9,058 matches, 0 wrong, 0 refusals. 2 answers were unavailable from the reference; 9 skim requests failed in transport. Steady-state answer rate was 100% for every kind. The head was stale 0.0% of the time; the longest gap between new heads was 26.7 s. RSS max was 250 MB.
- **Restarts:** 16. 13 recovered in 18-138 s. The startup BAL seeding ran every time (about 30 blocks in 1.5-3.7 s).
  - **265 s (graceful, config A, 12:09):** the chain was followed within 10 s and the snapshot replayed by 20 s. But something still needed SNAP, and no SNAP-capable peer connected until 12:13:58 (only reth and erigon peers). Ready 8 s after a Geth peer arrived. Which key waited isn't visible: `skim_subscriptionStatus` reports only a missing-key count.
  - **Never ready (graceful at 13:13, sigkill at 13:20, both config A)**, plus **9 samples over the key limit** (69 > 60, right after the switch back to A at 13:57). The cause is the archived `quote` call subscription. Archived keys are never released, so their history stays continuous. As the call's access set drifted, the store's subscribed set grew to 60 while the subscriptions needed only 45 (pool 7, promo 3, quote 33-35). From 13:09, admitting the call's new keys hit `max_subscribed_keys`, so `quote` was truncated and never ready again until config B. Restarts don't help: archived keys reload from the archive. The earlier soaks had the same drift (subscribed 56-59 against ~45 required) without reaching the limit.

**Fifth full soak (150 min, binary `01fd4fc`, head-only, macOS system allocator): recovery passes; FAIL on the key limit, no wrong answers.**
- **Answers:** 7,294 matches, 0 wrong, 1 refusal. 1,933 answers were unavailable from the reference, all of them in one ~200-block stretch (blocks ~11,943,100-11,943,300) across every probe kind, so it was the reference node (outage or rate limit). Answers in that stretch went unchecked.
- **Restarts:** 18, all recovered in 16-116 s (gate passes). With `skim_subscriptionStatus.missing` and the readiness log, any slow restart now names what it waited for.
- **Freshness:** stale 0.0%; longest gap between heads 24.6 s. **RSS max:** 194 MB.
- **Key limit, 75 samples over (up to 66 > 60):** each restart in config A archives a new `promo` account, and the previous ones were reloaded from the archive as subscribed keys without going through the limit, so the count grew restart by restart (35, 52, 56, 59, 60, 66). The release after 64 unneeded blocks never fired: restarts came every 30-60 blocks, and the wait is counted in memory. Fixed after this run: reloaded archive keys stay archived (writes recorded) but hold no subscribed slot until a subscription asks for them again (`archived_keys_reloaded_after_a_restart_hold_no_slots_until_asked_for`).

**Sixth full soak (150 min, binary `6a6de69`, head-only, macOS system allocator): PASS.**
- **Restarts:** 15, all recovered in 22-145 s.
- **Key limit:** never exceeded; subscribed keys peaked at 55 of 60. The archive keys reloaded at each start no longer take slots. The 64-block release still never elapsed between restarts, and doesn't need to.
- **Freshness:** stale 0.0%; longest gap between heads 24.7 s. **RSS max:** 168 MB.
- **Answers: mostly unchecked.** The reference node was rate-limiting this account (HTTP 429), so only 1,977 answers could be compared: 0 wrong, 2 refused. 7,433 were unavailable from the reference. The fifth soak, the same day, compared 7,294 with 0 wrong.

## A live blob-parameter fork on a devnet (TODO 24)
Devnet: `devnet/network_params.bpo.yaml` (three Nethermind + Lodestar, minimal preset, Gloas at epoch 4, a blob-parameter fork at epoch 12). skim started at epoch 6 from a finalized checkpoint, with CL peers from discovery only.

| Run | Binary | What happened |
|---|---|---|
| 1 | `25c2d2e` (swap topics at the fork) | Digest switched at epoch 12 (48875651 to 544789fd, matching Lodestar's ENR). All three CL peers dropped about a minute later and never came back; the head stopped. |
| 2 | `0fcf9d1` (topics overlap: next digest subscribed 2 epochs ahead, old one left 2 epochs after) | The overlap ran as planned, but the peers again dropped 60 s after the fork, 30 s *before* the old topics were left. So topic churn wasn't the cause. |
| 3 | `162eb48` (Status carries the current digest) | Peers kept (3 of 3) through the fork and after the old topics were left; the head kept advancing (block 137+, fork at ~96); `devnet/check.py`: all match. |

**Cause:** skim answered Status with the best status it had learned from peers, and that still carried the pre-fork digest. Peers re-check Status after a fork and dropped skim as being on another network. `status_to_send` stamps the digest in force (`our_status_carries_the_digest_in_force_after_a_fork`, mutation-checked). The overlap is kept: it is what the other clients do, and it avoids losing gossip at the boundary.

Also fixed on the way: light-client gossip updates were size-checked with mainnet constants; they now use the network's preset.

## How far back Sepolia peers serve history (`skim probe-retention`, 2026-10-19)
Seven eth/71 peers (6 Geth, 1 Nethermind), head 11,947,494. The Glamsterdam fork block is **11,856,337** (91,157 blocks back, about 12.7 days).
- **Every sampled block, from 16 back to the fork block itself:** all 7 peers served its BAL, body and receipts, each verified against the header.
- **Advertised earliest block (eth/69+ Status):** 0 or 1 on six peers; one Geth reports 1,450,409 (the Merge), so pre-merge history has expired there.
- **How long this lasts:** EIP-7928 requires ELs to keep BALs for the weak subjectivity period (3,533 epochs, about 15.7 days). Clients that prune at that minimum would drop Sepolia's fork-day BALs about 3 days after this measurement. After that, a post-fork archive can only come from someone who kept them, or from re-executing the blocks on an archive node (the result must match `block_access_list_hash`).

## State edge cases against an executing node, and the post-BPO fork id (2026-10-19)
Nethermind's BAL catch-up ([PR 14157](https://github.com/NethermindEth/nethermind/pull/14157)) tests state reconstruction over account creation, a touched missing account, create + selfdestruct, slot clearing, 7702 set/clear and reverts. The same cases ran on the BPO devnet (`examples/devnet-edgecases.mjs`), with skim subscribed to every affected key beforehand and Nethermind executing: 11 steps, 9 accounts, balance/nonce/code/4 slots each, all equal at each step's block hash ([devnet README](../../devnet/README.md#state-edge-cases)).

Getting there found a bug: after the devnet's blob-parameter fork (BPO3, after Amsterdam), no new EL session formed. reth builds a genesis's fork list in mainnet's order (BPO3 before Amsterdam) and folds timestamp forks into the EIP-2124 fork id in list order, so skim's id matched no peer once both forks were active. Nethermind closed the session (`ProtocolBreach`) and then refused the address at the ECIES handshake. The afternoon's live-fork run hadn't shown it: its sessions were made before the fork. Fixed by ordering timestamp forks by activation time (`a1ac222`, `a_blob_parameter_fork_after_amsterdam_folds_into_the_fork_id_in_activation_order`, mutation-checked). Sepolia schedules no fork after Amsterdam, so production wasn't affected.

## Hedged fetch: adaptive vs fixed, on public Sepolia peers (2026-10-19)
One node (build `d72e0ae`, `--debug-rpc`, macOS, 5+ SNAP peers), 3,600 cold reads of never-read keys (random accounts with `eth_getBalance`, random WETH slots with `eth_getStorageAt`), sequential; the hedging mode switched at random per read (`skim_debugSetSnapHedge`), so both modes saw the same peers at the same time ([`tools/hedge_ab.py`](tools/hedge_ab.py)). Two nodes side by side were tried first and dropped: after 20 minutes they had 3 and 9 SNAP peers, which would have measured peer luck.

| Mode | Reads | p50 | p95 | p99 | Max | SNAP requests per read | Hedges |
|---|---|---|---|---|---|---|---|
| adaptive | 1,812 | 46-49 ms | 100-119 ms | 112-163 ms | 330 ms | 1.00 | 1 |
| fixed (1.2 s) | 1,788 | 46-48 ms | 101-115 ms | 112-135 ms | 248 ms | 1.00 | 0 |

(Ranges: the two batches, 600 and 3,000 reads.) No peer stalled in these runs: the slowest read was 330 ms, far below the fixed 1.2 s threshold, so neither mode had a tail to cut, and adaptive hedging cost nothing (one hedge in 3,600 reads). Its benefit, a stalled request re-sent after ~150 ms instead of 1.2 s, is shown by `a_stall_at_a_fast_peer_is_hedged_after_its_own_latency_not_a_fixed_delay` but wasn't observed live, because the condition didn't occur. Earlier runs did record stalls (worst on-path fetch 1.6 s, [M2](m2-p0-miss-tail.md)); a long soak with the hedge counter is where they'd show.

**With injected faults** (review 18 asked to separate this from live soaks): `hedging_under_injected_stalls_and_drops` in `skim-snap` (an `#[ignore]` benchmark: `cargo test -p skim-snap --release -- --ignored --nocapture hedging_under_injected`). Three mock peers answer in 10 ms; deterministically, every 33rd request stalls 2 s (~3%) and every 97th fails at once (~1%). 600 reads in waves of 6, after 30 warm-up reads:

| Mode | p50 | p95 | p99 | Max | Requests per read |
|---|---|---|---|---|---|
| adaptive | 13 ms | 23 ms | **187 ms** | 1,239 ms | 1.04 |
| fixed (1.2 s) | 13 ms | 15 ms | **1,233 ms** | 1,240 ms | 1.05 |

With stalls present, adaptive hedging cuts p99 from ~1.2 s to under 0.2 s at no extra request cost (hedges replace the retries fixed mode needs anyway). p95 is a little higher (hedged reads land at 150-190 ms, near the boundary), and the maximum is unchanged: a few stalls weren't rescued early, plausibly because the one hedge a job gets landed on a stalling request too. Live, the effect still waits on a soak with real stalls (TODO 27).

