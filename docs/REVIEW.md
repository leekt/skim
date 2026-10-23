# Code review, 2026-10-23

A whole-codebase review of the working tree at `df686b7` plus the uncommitted Hegota changes
(`forks.rs`, `frame.rs`, Heze digests). Two independent reviews are consolidated here: five
per-subsystem Claude reviewers, and a Codex review that also covered product positioning. Every
high-severity item was re-read in the source before being listed. Findings both reviews raised
are merged and marked **(both)**.

Baseline: 218 workspace tests pass (1 ignored), 18 example tests pass; clippy reports ~60
style-only warnings. No live-network soak or new fuzzing was run for the review.

Status: **fixed** (in the working tree, with a regression test where one is practical),
**doc** (the documentation was corrected instead), **open** (left for later; the ones needing
design work are [TODO](TODO.md) items 30-35).

## Fix pass

Fixed in the same working tree, after the review: 37 of 47 findings fully or in part,
22 new regression tests (workspace: 218 → 240 passing, 1 ignored), and the docs updated
where behaviour changed (`json-rpc.md`, `trust-and-correctness.md`, `use-cases.md`, `hegota.md`,
PROJECT.md §3.2).

The fuzz crate didn't build: `fuzz_targets/light_client.rs` predated the `LcConfig`/
`ForkVersions` API (`6fd23e3`). It's updated and builds; the targets weren't run (the installed
nightly is older than the dependencies' MSRV), so `rpc_chunks` over the new chunk limits is
covered by unit tests only.

Two findings changed on closer reading:
- **R3** is lower severity than first reported. revm caps one transaction's execution gas at
  2^24 (EIP-7825, with EIP-8037's reservoir for state gas only), so a single call never runs
  unbounded. The real exposure was amplification: `eth_simulateV1` had no limit on blocks, calls
  or total gas. Both the cap and the limits are now in.
- **N1**: relaying nothing would cost mesh standing (peers score mesh deliveries), so payloads
  that pass the commitment check are still relayed. The builder signature remains unchecked
  (no beacon state); that residual risk is stated in PROJECT.md §3.2.

## Overall

The core holds up. Unavailable, proven-absent and present stay distinct from SNAP through the store
and the simulator; no path was found where missing state reads as zero or leaks between
simulations. The Merkle proof walker, BAL commitment checks, light-client verification
(branches, gindices, domains, committee rotation) and hash-linked EL recovery are sound, and the
tests show sustained attention to correctness.

The weak spots are at the edges, where internal guarantees meet the wider interfaces:

- **P2P trust boundary.** skim relays gossip it hasn't validated, accepts an optimistic envelope
  as a reorg of any depth, and lets single peer responses grow without a byte budget.
- **RPC surface.** Resource limits (gas, batch size, subscriptions) and some guarantees
  (reorg guard, `--require-verified`, log removal on reorg) don't reach every path.
- **Archive durability.** A re-anchor or a resubscription can destroy or hide history that peers
  no longer serve.
- **Async boundaries.** The next testing investment should target reorgs during requests,
  recovered blocks during subscriptions, and archive release/reactivation.

## Findings

### P2P and consensus

| ID | Sev | Finding | Status |
|---|---|---|---|
| N1 | High | **(both)** Gossip relayed before validation. `skim-cl/src/node.rs:1100-1129` reports `execution_payload` as `Accept` after parsing (before the commitment checks), `beacon_block` after parsing (no proposer signature) and any 240-byte `beacon_attestation_*` message; nothing is ever rejected. Peers that validate penalise skim as the source of invalid messages, and a flood is amplified. | fixed: only payloads passing the commitment check are relayed; blocks and attestations never |
| N2 | High | Optimistic envelopes can roll the chain back to any depth. `skim-cli/src/chain.rs:86-95` accepts any envelope whose parent is stored (up to `--window`) as a one-block fork; `verify_payload` checks internal consistency only, and in `skim run` nothing links the envelope to a signed bid (PROJECT.md §3.2 says it does). One forged envelope with `parent = head-200` makes `latest` serve invented state until the next canonical block. | fixed: only a sibling of the head switches branches; doc corrected (D2) |
| N3 | High | Status poisoning. `learn_status` (`node.rs:695`) adopts a peer's finalized root when the epoch is *equal*, with no bound against the current epoch. One peer can make skim advertise a wrong finalized root, and honest peers disconnect it as irrelevant. | fixed: forward only, ≤ current epoch, two peers must agree (an explicit peer suffices) |
| N4 | Med | Request ids collide across protocols. Four request-response behaviours share one `env_reqs` map keyed by `OutboundRequestId`; each behaviour numbers from 1, so a bootstrap and an envelope request in flight together drop one reply and misroute the other. | fixed: keyed by (protocol, id) |
| N5 | Med | Peer-controlled allocation in CL responses. `skim-cl/src/rpc.rs:265-344` buffers up to 256 MiB and decodes any number of chunks, each `vec![0; len]` sized by the peer's varint. | fixed: per-protocol chunk count, chunk size and total; buffers grow with decoded data |
| N6 | Med | Light-client gossip floods cost a BLS pairing each, run under the committee mutex in spawned tasks, and the sender is never penalised (`node.rs:1125`, `skim-cli/src/run.rs:568`). | fixed: at most 4 checks in flight, the rest dropped (counted) |
| N7 | Med | One slow consumer stalls the swarm: the event channel is bounded by count (4096), and the swarm loop awaits `send`, so Ping/Status/heartbeats stop. | fixed: `try_send`, queue of 256, payloads failing the check never queue |
| N8 | Med | **eth/70 receipt continuation is unbounded and can panic.** `skim-el/src/lib.rs:805-860` appends partial receipts while the peer keeps saying "incomplete" (no byte cap before the root check; ~GBs per call with `buffer_unordered(16)`), and `blocks_receipts` slices `hashes[out.len()..]` after a peer returns more lists than requested: a peer-triggered panic in `eth_getLogs`. | fixed: per-block budget from `gas_used`; surplus lists truncated |
| N9 | Low | Served envelopes carry the current digest even for pre-fork envelopes still in the 64-slot cache (`skim-cl/src/rpc.rs:356`). | open |
| N10 | Low | ENR `nfd` is always zero; `NodeConfig::next_fork` ignores BPO epochs. | open |
| N11 | Low | `ForkVersions::at_slot` falls back to the first entry for earlier slots. | open |
| N12 | Low | `bid()` doesn't check that offsets point past the fixed part (probe-only today). | fixed |
| N13 | Low | Frame receipts served to eth/66-68 peers fall back to legacy encoding (`frame.rs:617`); the mempool probe trusts the claimed sender of a type-6 transaction on every network (`frame.rs:369`). | fixed: such blocks end the eth/66-68 answer; frame transactions are skipped by the probe |
| N14 | Low | `receipts70` serving can return an empty partial with `last_block_incomplete = true` forever when one receipt exceeds 2 MiB (`serve.rs:233`); snap/1 requests are never answered (`snap1.rs:185`). | fixed: at least one receipt per answer; snap/1 requests get empty answers |
| N15 | Low | `GetByteCodes` replies with no codes score as fast successes (`skim-snap/src/lib.rs:686`). | fixed: empty answers aren't rewarded |
| N16 | Low | Whole-storage download doesn't verify each page's range proof, only the final root (`skim-snap/src/el.rs:166`); with an unlimited slot limit a peer can grow memory without bound. | open (TODO 34) |

### Chain following and state

| ID | Sev | Finding | Status |
|---|---|---|---|
| C1 | High | A re-anchor wipes the archive. `Reconcile::NoAncestor` calls `rollback(0)`, which runs `archive.truncate_above(0)` (`skim-state/src/lib.rs:787`) and deletes every archived block, version and coverage, including backfill that peers no longer serve. | fixed: `Store::reanchor` keeps the archive below the new block; the healer reconciles |
| C2 | High | The follower stalls permanently after a gap of ~70+ blocks: `reconcile` asks for the whole gap in one header and one BAL request (peers cap both), and the CL fallback walks back only 72 slots. Every later block is `Deferred`, logged at debug level. | fixed: chunked (64), CL walk sized to the gap, re-anchor past 1,024 blocks, warning on repeated deferrals |
| C3 | Med | Rollback drops a whole-storage download (`complete.retain`, `lib.rs:796`) but leaves the address in `watched_addresses`: writes keep being recorded outside the subscription budget. | fixed: released like any dropped download |
| C4 | Med | The store's write lock is held across redb fsync commits (`write_block`, `put_code`, `write_base`), stalling every read. | open (TODO 31) |
| C5 | Low | Every SNAP-fetched bytecode is archived forever when `--archive` is on (`lib.rs:957`). | fixed: only code an archived account points to |
| C6 | Low | `from_bal` and `before()` pick post-values differently (last element vs highest index), and `decode` doesn't check canonical ordering. | open |
| C7 | Low | A block applied without a noted header is archived with empty `header_rlp`; `archived_block` reads rlp and hash in separate transactions. | open |
| C8 | Low | `archive_try` swallows `write_base` errors, so memory says archived while disk doesn't. | open |
| C9 | Med | **(Codex)** Resubscribing an archived key replaces its coverage start (`skim-state/src/archive.rs:667`): history recorded before the release becomes unreadable. Intended, but it breaks the durable-history expectation; coverage should be a set of intervals. | open (TODO 30) |
| C10 | Low | `Placed::Reanchored` isn't handled as a head change: the first block of every run logs a spurious "re-anchored" warning and skips `NewHead`, maintenance and `light.on_block`. | fixed |
| C11 | Low | After a fork stop (`forks.rs`), `latest` keeps answering from frozen state with success codes. | fixed: `latest`/`pending` answer `fork_unsupported` |

### JSON-RPC and streams

| ID | Sev | Finding | Status |
|---|---|---|---|
| R1 | High | **(Codex)** The reorg guard samples too early. `reorg_guard(method, epoch, self.reorg_epoch(), self.dispatch(..).await)` (`rpc.rs:849`) evaluates `reorg_epoch()` *before* the await, so a reorg during the request is never detected. | fixed |
| R2 | High | **(both)** WebSocket `logs` subscriptions never send `removed: true` on reorg, emit only the announced head (intermediate blocks of a gap fill are skipped), and silently skip a `Lagged` broadcast (`rpc.rs:1305-1320`). Deposit monitors end up with wrong projections. | fixed: per-session chain view (`ws_feed.rs`), up to 64 missed blocks caught up |
| R3 | High | `eth_estimateGas` replaces its gas cap with the caller's `gas` (`rpc.rs:588`): one call to a looping contract with `gas = 2^64-1` holds a CPU permit indefinitely; a few block every `eth_call`. `eth_simulateV1` has no block/call count limit and takes a user gas limit. | fixed: `eth_call` cap; simulate: 256 blocks, 1,000 calls, shared gas budget (severity: see Fix pass) |
| R4 | High | `eth_getLogs` clamps `fromBlock` to the head (`chaindata.rs:1798`): an indexer polling `last+1 .. latest` gets the head block's logs again. | fixed |
| R5 | Med | No limits on HTTP batch size, WebSocket subscriptions per connection, or connections. | fixed: 1,000 per batch, 128 subscriptions per connection (connections not limited) |
| R6 | Med | Default CORS `*` and no WebSocket Origin check: any web page can read `skim_status`/`skim_dashboard` (watched addresses, peer ids). | open (TODO 32) |
| R7 | Med | `--require-verified` doesn't cover `skim_streamNext` (uses `store.head()`) or WebSocket `newHeads`/`logs`. | fixed |
| R8 | Med | **(both)** Overrides accepted but ignored: `eth_call` drops `blockOverrides`; `eth_estimateGas` drops state overrides; `eth_simulateV1` applies only part of `BlockOverrides` (`multi.rs:90`: `blobBaseFee` and `blockHash` are ignored). | fixed: `eth_call` block overrides, `blobBaseFee`, `blockHash`; `beaconRoot` and `eth_estimateGas` overrides refused |
| R9 | Med | `BLOBBASEFEE` uses revm's Prague update fraction (`env.rs:37`), not the chain's current blob schedule (BPO2 on Sepolia). | fixed: from the chain's blob schedule |
| R10 | Med | The RPC server's bind error is dropped (`run.rs:503`): a busy port means a node with no RPC and no error. | fixed: bound before startup continues |
| R11 | Med | Predictor panics on crafted calldata: `u256_word(..).to::<u32>()` (`skim-sim/src/predictor.rs:85`); V3 pools with tick spacing 0 divide by zero (`prefetch.rs:294`); `fee().to()` in `run.rs:727`. | fixed |
| R12 | Med | A single call can trigger tens of thousands of SNAP fetches: no per-request key budget; learned account waves aren't capped. | partly: predicted account waves capped; miss rounds open (TODO 35) |
| R13 | Low | `eth_getLogs` rejects `safe`/`finalized`, which every other method resolves through the light client. | fixed |
| R14 | Low | `--max-logs-range 0` panics in `clamp` (`stream.rs:157`); `--window` isn't validated. | fixed |
| R15 | Low | `eth_sendRawTransaction` accepts trailing bytes (`decode_2718` instead of `decode_2718_exact`). | fixed |
| R16 | Low | A reorg or window expiry mid-simulation turns into a re-fetch loop (`db.rs:129` maps `Reorged`/`OutOfWindow` to a miss) and finally `Incomplete` instead of `Reorged`. | fixed |
| R17 | Low | Multi-block `eth_simulateV1`: `BLOCKHASH` of an earlier simulated block never resolves; timestamps ignore a previous block's override; `validation: true` doesn't check cumulative gas. | fixed, except system calls in simulated blocks (open) |
| R18 | Med | **(Codex)** Duplicate subscription names silently merge (`subscription.rs:259`): two subscriptions on different accounts become one, and readiness reports the survivor. | fixed |

### Docs and positioning

| ID | Finding | Status |
|---|---|---|
| D1 | **(Codex)** `use-cases.md:150` advertises "accounting that needs every ETH movement". BALs record post-transaction values: ETH received and forwarded within one transaction leaves no balance change (EIP-7928 omits unchanged balances). BAL streams are net state monitoring; complete transfer accounting needs events or traces. | doc |
| D2 | PROJECT.md §3.2 says `skim run` checks the payload's link to the beacon block's bid; only `probe-cl` reads the bid. | doc |

## Product and design recommendations (Codex)

- **Lead with the application sidecar.** Call subscriptions let users declare their workload
  without learning storage layouts. Onboarding should be one complete workflow: configure calls,
  wait for readiness, consume results, handle unavailable state, recover after restart.
- **Readiness as policy.** Freshness, subscription completeness, consensus authentication and
  peer availability are separate conditions; several unusable states produce warnings while
  `skim status` exits 0. A readiness check should take required subscriptions, a maximum head age
  and required authentication, and fail accordingly.
- **Measure the whole resource envelope.** State is selective, but gossip, payload processing and
  recent-BAL retention scale with chain activity, and scheduler concurrency limits don't bound
  queued demand. Add admission limits and request deadlines, then measure cold bursts,
  subscription churn, archive contention and heavier chains before positioning skim as shared RPC.

| Use case | Fit |
|---|---|
| Repeated quotes and simulations over known contracts | Strongest: predictable working sets benefit directly from subscriptions and prefetch |
| Protocol dashboards and keepers | Strong, with explicit freshness and authentication requirements |
| Selective indexers (e.g. smallname) | Good, using durable streams and visible completeness limits |
| Historical accounting and backtesting | Conditional on dependency coverage and preserved archive intervals (C9) |
| Generic wallet backend or shared RPC | Conditional: cold latency, transaction lookup scope, admission control (R3, R5, R12) |
| Broad explorers, arbitrary historical queries | Weak fit for the coverage model |

## Structure

- `rpc.rs`: `dispatch` is one ~370-line match plus ~400 lines of inline status JSON; split into
  eth / debug / skim / ws modules. The module doc is stale on `safe`/`finalized`.
- `run::run`: ~400 lines mixing setup and the event loop; an event-loop struct holding the
  per-block side effects would have made C10 visible.
- `chaindata.rs`: four near-identical "passes × peers, fetch, verify, cache" loops could share one
  retry helper.
- `skim-state` ownership is spread over `watched_addresses`, `watched_keys`, `temp`, `complete`
  and `archived`, kept consistent by hand (C3 is one drift). An explicit ownership enum per key
  would make that drift unrepresentable.
- "Fork at epoch/slot" is implemented three times in `skim-cl`; the EVM spec is fixed at Amsterdam
  rather than taken from the chain's fork schedule (R9, and Bogotá under `--experimental-hegota`).
