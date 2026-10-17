# M6: correctness and bounds under restarts, reorgs and churn

Runs: 2026-10-15 (KST), Sepolia.

**Goal:** after an external review, prove that long runs on small resources never return a wrong value, instead of adding RPC methods.

## Review findings fixed (each with a regression test)
| Finding | Fix | Test |
|---|---|---|
| Archive: versions written by `write_base` (late-archived keys, replayed writes) and by backfill were not journaled, so a reorg kept the old fork's value | Every stored version is journaled; rollback also withdraws coverage that starts above the rollback point | `reorg_removes_late_base_and_replayed_versions` (fails without the fix) |
| `BLOCKHASH` read an unknown hash as zero; a 256-block window lost H−256 | Unknown hash = miss, resolved from hash-linked ancestor headers or refused; a 257-entry hash ring independent of `--window` | `blockhash_ring_outlives_a_short_window`, `ancestor_hashes_are_accepted_only_when_hash_linked` |
| `max_subscribed_keys` trimmed bookkeeping only; fetched keys were already subscribed; bytecode never freed | Limit enforced by the store at admission for every path; over-limit keys stay on demand; declared keys over the limit fail at startup; bytecode GC'd with its accounts | `subscribed_key_limit_is_enforced_at_admission`, `bytecode_is_dropped_with_its_last_account`, `declared_keys_over_the_limit_are_rejected_at_startup` |
| A simulation subscription whose call failed learned no keys and read as ready | Ready requires every call to have run cleanly at that block, nothing truncated | `failed_or_truncated_simulations_are_never_ready` |
| `eth_call` ignored `gasPrice`, `accessList`, `authorizationList` | Honoured; blob fields or conflicting fee fields rejected (`-32602`) | live: 354/354 `GASPRICE` probes matched (below) |

## Chaos soak (`docs/experiments/tools/chaos_soak.py`, 2 h)
Every block, skim and a reference node answered the same requests at the same block hash. Each answer was classified:
- **match:** skim's answer equals the reference's
- **refused:** skim returned an error such as `-32002`
- **WRONG:** skim returned a value the reference contradicts

Every 6–12 minutes the soak either killed skim (SIGKILL), restarted it gracefully, or restarted it with the other config (a different key set). The archive file persisted throughout.

**Setup:**
- `max_subscribed_keys: 60`, `--window 32` (so archived reads come from disk)
- a local full node pinned as the state peer (`--trusted-peer`; its answers are still Merkle-checked)
- a separate reference node

Partway through, the binary under test was rebuilt with restart persistence, so later restarts also exercised the snapshot path.

| | Result |
|---|---|
| Blocks | 355 |
| Disruptions | 12 (5 graceful restarts, 4 SIGKILL, 3 config switches) |
| Answers | **3,465 match · 23 refused · 0 wrong** (3 where the reference itself errored) |
| Random V3 quotes / subscribed quote | 1,036 / 350 match, 4 / 3 refused |
| Subscribed / cold slots | 707 / 342 match, 3 / 13 refused |
| `BLOCKHASH(n−k)`, random k in 1..256 | 350 match |
| `GASPRICE` with an explicit price | 354 match |
| Archived slot at older blocks | 326 match |
| Subscribed keys | max 60 (limit 60), 0 samples over the limit; up to 177 admissions refused |
| Memory (RSS) | P50 85 MB, max 102 MB |

The refusals are reads during the seconds after a restart, before state was fetched again. They are correct behaviour: unknown is refused, not guessed.

## Restart persistence
skim keeps a snapshot of subscribed state (values at a block, plus referenced bytecode), its EL node key, and known peers in `--data-dir`. On start the snapshot is used only after the chain from its block to the head is verified:
1. headers link by hash at both ends
2. every BAL matches its header
3. those blocks' writes are replayed

Otherwise it is discarded. Tests cover a snapshot from an orphaned fork, a chain that ends at the wrong head, and a chain with a gap.

**Restart timing** (`data/persist-test.sh`, Uniswap V3 config, public peers):

| Run | Cold (no snapshot) | With snapshot |
|---|---|---|
| 1 | 183 s | 138 s, 72 s |
| 2 | 74 s | 121 s, 231 s |

Both runs are dominated by waiting for the first EL peer that serves eth/71. Replaying the snapshot itself takes about 1 s. With so few public EL peers, a snapshot doesn't reliably speed up restarts, so catching up from consensus peers is being added (below).

## Catching up from consensus peers
Gloas `ExecutionPayloadEnvelopesByRange` returns recent payloads with their BALs, and consensus peers connect within seconds. skim now requests the restart gap this way first and verifies each envelope exactly like gossip. It falls back to EL peers only if that fails.
- **Probe** (`skim probe-cl-req`): peers served 8 envelopes per request, 100–250 KB each.
- **First integration attempt:** failed because skim only asked peers whose handshake head covered the whole range, and at startup none did. Fixed.
- **Subscription maintenance:** it now waits while a restore runs, instead of competing for EL peers.

**Restart timing with consensus catch-up** (same test):

| Round | Ready after |
|---|---|
| Cold (no snapshot) | 83 s |
| Snapshot | 36 s |
| Snapshot | 53 s |

The snapshot replay itself took 2.4–2.8 s from consensus peers. Most of the remaining time is waiting for the first gossiped block (~20 s) and the subscription pass.

## Consensus light client
Implemented: the Altair sync protocol with Gloas containers.
- **Bootstrap:** from a checkpoint root, with the sync-committee branch checked at gindex 2945.
- **Updates:** optimistic and finality updates require a 2/3 supermajority BLS aggregate signature, and the execution block hash is proven against the beacon body root at gindex 2856.
- **Committee rollover:** through signed finalized headers.
- **RPC:** `safe` and `finalized` block tags, `--require-verified`, and status in `skim_status`.
- **Unit tests:** Merkle branches, header roots, and BLS aggregate verification, including rejection of a wrong header, a wrong fork version, and too few signers.

**Live status: blocked by the network, not by skim.**
- **Bootstrap:** Sepolia peers (Lighthouse v8.3.0-rc.0) answer *"Sync committee branch for block root … not found"*, even for the current finalized checkpoint.
- **Gossip:** no light-client update arrived on either topic in 3 minutes.
- **Local Lighthouse:** with its light-client server enabled (the default), it returns 404 for the finality update, the optimistic update, and the bootstrap.

Consensus clients don't produce Gloas light-client data yet. Until they do, skim reports this in `skim_status` and answers `safe` / `finalized` with `-32002`. Nothing is assumed.

## Consensus gossip mesh A/B
Mesh 12 / 80 peers against 8 / 50, measured on the same 304 blocks:

| Metric | Old (8 / 50) | New (12 / 80) |
|---|---|---|
| Arrival after slot start, P50 | 854 ms | 760 ms |
| Arrival after slot start, P90 | 1,601 ms | 1,671 ms |

The median per-block difference between the two was −17 ms (new earlier in 56% of blocks). The payload-topic mesh stayed at 6 peers either way, because remote peers decide whether to graft us. The defaults stay at 8 / 50.

## Second review (findings on the M6 code)
| Finding | Fix | Test (fails without the fix) |
|---|---|---|
| Backfill extended a late key's base (post-state of block c) back past c without checking whether c itself wrote the key (K was 10 before 100, 20 after; backfill claimed 20 for 90–99) | Block c's BAL is part of the scan. If c wrote the key and nothing earlier did, coverage stays at c (write-bound). Keys whose start block a previous backfill proved clean are tracked (`PRE_CLEAN`), so deeper passes need not rescan c | `backfill_checks_the_base_block_itself` ✓ |
| Archived snapshots read by block number only, so a handle for A100 could read B100 after a reorg | Archive reads take the expected block hash and check it in the same read transaction (`get_at`); mismatch is `Reorged` | `reads_for_a_replaced_block_are_refused` |
| `insert_base_with` checked the fork, released the lock, then wrote the archive, so a rollback in between left old-fork data | Every archive mutation (base writes, key archiving, rollback truncation) happens under the same store write lock as the in-memory change | `a_base_write_and_a_rollback_never_interleave` (a test hook pauses the write while another thread rolls back and switches forks) ✓ |
| `archive_key_checked` admitted under a read lock and inserted later (two callers could take the last slot) | Admission and insertion under one write lock | `archive_admission_and_insert_are_atomic` (8 threads, limit 50 → exactly 50) |
| On-demand keys had no cap; recent BALs are bounded only by block count | `limits.max_on_demand_keys` (default 200k; soonest-expiring evicted). The recent-BAL index is reported (`skim_status.store.recent_bal_bytes`), and `--window` bounds it. No single total-memory cap is claimed | `on_demand_keys_are_capped` |
| The soak counted a reference revert as "reference unavailable", hiding success-vs-revert and different-revert mismatches; no failing exit | Classified by execution outcome (result / revert data / EVM halt) vs unavailability; nonzero exit on WRONG, limit breach, or RSS over `RSS_LIMIT_MB`; `--selftest` with the review's cases | `chaos_soak.py --selftest` 10/10 |

✓ = also checked to fail against the previous code.

**What this means for the first soak:**
- The old classifier could hide a mismatch only where the reference returned an error.
- That happened 3 times, all `BLOCKHASH` probes (contract creations that cannot revert). The raw responses weren't logged, so those 3 can't be re-classified.
- Every other answer was a value or a skim refusal and was compared correctly. The 0-wrong result holds for those 3,488.
- The next soak runs with the corrected classifier.

Reorgs are covered by the deterministic store tests, not by the live soak.

## Serving the network
Until now skim only took from the network: inbound eth requests were dropped, and consensus range/root requests got empty answers. It now answers from data it has already verified (see [how it works §7](../guide/how-it-works.md#7-what-skim-serves-back)):
- **Execution layer:** headers, bodies, receipts and eth/71 BALs, plus snap/2 bytecode and BALs.
- **Consensus layer:** payload envelopes by range and by root.

Every item is re-checked against its header before it is sent, and envelopes are served only from the chain skim applied.

**Probe** (`skim probe-serve`): a separate process connects to one skim node only (EL as its sole trusted peer, CL as an explicit peer), then requests and verifies everything independently:

| Request | Result |
|---|---|
| `GetBlockHeaders` (falling from the advertised head) | 13 / 13, hash-linked (all blocks the node had followed) |
| `GetBlockAccessLists` | 13 verified against `block_access_list_hash`, 0 wrong |
| `GetBlockBodies` | 13 verified (transactions root and withdrawals root), 0 wrong |
| `GetReceipts` | blocks the node had fetched for an RPC user: verified against `receiptsRoot`; others: "not held" |
| snap `GetByteCodes` (one held hash plus one unknown) | 1 returned, matching its hash |
| `ExecutionPayloadEnvelopesByRange` (16 slots) | 11 returned (the other slots were empty or before the node started), all verified and on the EL chain |
| `ExecutionPayloadEnvelopesByRoot` (one known root plus one unknown) | 1 returned, verified |

**Fixed along the way:** the EL handshake advertised a head about 100 blocks behind, taken from a peer-polling loop left over from the probes, so peers asked for blocks skim didn't hold. skim now advertises the block it last applied, and `BlockRangeUpdate` covers the range it holds without gaps.

**Real peers** (35 min on Sepolia, 4–10 EL peers): few requests arrived, because few public peers speak eth/71 and consensus clients range-sync from peers that advertise history (skim's status still advertises none). One consensus peer fetched an envelope by root, and a few EL peers asked for headers. The serving path matters most for other skim instances, which fetch BALs, headers and bodies over eth/71 when they restart or miss gossip.

**Cost:** about 10–13 MB of RSS. Two instances with the Uniswap V3 config ran side by side, one with `--no-serve`, and were compared at the same age:

| Age | Serving | `--no-serve` |
|---|---|---|
| ~9 min | 113 MB | 100 MB |
| 24 min | 151 MB | 141 MB |

The caches are bounded: BALs of 64 blocks, bodies of 128, and envelopes of 64 slots.

**What memory grows with:** both instances grew about 2 MB per minute until the 256-block `--window` filled (51 min; the recent-BAL index alone was about 42 MB). After that, growth slowed to about 0.3 MB per minute (202 MB at 71 min), as the bounded header and transaction indexes keep filling. `--window 32` keeps a node near 100 MB (chaos soak above).

## On-demand reads right after start
**Symptom:** `eth_getCode` (and every other read of state skim doesn't track) failed with `-32002 state still incomplete after 1 fetch rounds`, after exactly 15 s. This happened for about the first 1–2 minutes after start.

**Cause:** no SNAP peer was connected yet. Consensus gossip makes the head ready within seconds, but on public Sepolia the first SNAP-serving EL peer took 65 s and 2:08 on two cold starts. Each fetch waited out the scheduler's 15 s deadline, then reported a generic failure.

**Fixes:**
- **Wait for a state peer, then answer.** An interactive request that needs state checks for a SNAP peer before each fetch wave. If none is connected, it waits for one (`--state-wait`, default 60 s) and then fetches normally. Before, the call burned its fetch rounds against an empty peer set: 8 rounds, each lost to the deadline, taking 55.8 s and then returning `-32002`. Only if no peer arrives in time does it answer `-32002` *"no SNAP state peers connected after waiting …"*. Live, on a cold start: three `eth_call`s issued at the first block (WETH `balanceOf`, USDC `totalSupply` and `balanceOf`) waited 35–37 s for the first SNAP peer, then returned values identical to a reference node at the same block. State never comes from gossip here: gossip carries only each block's writes, and skim already applies those.
- `skim_status.snap_fetch` exposes the scheduler's counters and per-peer stats.
- **Saved peers fixed:** before, only the peers connected at save time were kept (often one), at their *connection* address. For a peer that had dialed in, that is an ephemeral port, which can't be dialed back. Now skim saves every eth/69+ peer it dialed during the run, SNAP-capable first, merged with the previous list (up to 64).

**Restart timing with saved peers:**

| Restart | Saved peers | First SNAP peer |
|---|---|---|
| 1 | 10 | 17 s |
| 2 | 12 | 2:33 (the saved SNAP peers were full) |

Subscribed state didn't wait in either restart: the snapshot replayed from consensus peers in about 2 s.

Once a SNAP peer was connected, cold reads of four unsubscribed contracts (code and balance) answered in 0.26–1.26 s.

## Third review (state lifecycle)
| Finding | Fix | Check |
|---|---|---|
| Direct reads (`eth_getStorageAt`, `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`) didn't extend an on-demand key's TTL on a cache hit, so a value polled every block expired and was fetched again every few blocks. `eth_simulateV1` and tracing had the same gap | Every read path extends the TTL of the on-demand keys it used | Live: an unsubscribed account's balance polled for 10 blocks (TTL 4): 2 SNAP requests in the first block, 0 after |
| Uniswap V3 background prefetch stored its tick window as **subscribed** keys, forever. Ticks the price left behind piled up, used the `max_subscribed_keys` budget (so a subscription's own keys could be refused), and survived restarts through the snapshot | The window is a hot set: keys in it get their TTL extended on each pass and expire `window_ttl_blocks` (16) after the price moves away. Restored snapshot values are provisional until a subscription claims them | Live, after a restart: subscribed keys **3,166 → 35** (the two subscriptions' own keys); the window's ~3,100 keys are on demand; both subscriptions ready |
| `storage: all` checked `max_slots` only at download. Slots first written later were tracked without counting toward any limit | Those slots count toward `max_slots` and `max_subscribed_keys`. Past `max_slots`, the contract is no longer tracked and the subscription reports the error (no re-download every block, also for a contract too large at download) | `complete_storage_answers_every_slot_and_follows_writes` |
| A subscription pass still running made the next block's pass be skipped, so readiness and `on_block` results lagged | Passes are coalesced: one at a time, always on the newest head | |
| Readiness counted an account's code hash but not its bytecode, so a "ready" subscription could still fetch code on the first call | Missing bytecode of subscribed accounts is fetched and counts as missing | |

**BAL-first resolution (new).** A key skim doesn't hold is first looked up in the recent-BAL index. If a block in the window wrote it, and every block since is indexed, that write is the value at the head, with no fetch. Each block's writes are kept sorted, so a lookup is a binary search per block.

Live check: the balances of the 10 senders in the latest block (none tracked), read through skim:
- all 10 identical to a reference node at the same block
- 0 SNAP requests
- `resolved_from_bal` 0 → 10

Tests: `recent_bal_answers_only_inside_the_indexed_window` (before the write: unknown; a later write wins; outside the window: unknown), `recent_bal_lookup_follows_reorgs`.

**Restarts no longer wait for the snapshot replay.** A restart snapshot 3,900 blocks old was replayed from EL peers in 175 s, and subscriptions waited for it.
- **Change:** subscription maintenance now starts at the first block and fetches what it needs fresh, while the replay runs alongside. When the replay finishes, it fills in only the keys still unknown; anything a fresh fetch already resolved is left alone (`a_restore_fills_only_keys_a_fresh_fetch_has_not_resolved`).
- **Why it pays now:** since the V3 fix, subscribed key sets are small (35 keys here), so a fresh fetch is one SNAP round. Replay costs a request per 64 blocks of gap.

Live, with a snapshot 655 blocks old:

| | Before | After |
|---|---|---|
| Subscriptions ready | after the replay (79 s here; 175 s for the 3,900-block gap) | **25 s** after start (first block at 20 s) |
| Replay | blocks readiness | finished at 79 s in the background, installed the 2 keys still missing |
| Pool `slot0` / liquidity and the subscribed quote vs a reference node | | identical, both before and after the replay landed |
