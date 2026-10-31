# skim

**A P2P-backed Ethereum state and simulation client for applications.**

This document describes the implemented architecture as of 2026-10-19. Operator instructions live in the [guides](docs/README.md); measured runs remain in [experiment logs](docs/README.md#experiment-logs), and unfinished work is listed in [TODO](docs/TODO.md).

| | Current implementation |
|---|---|
| Status | Experimental; BAL follower, verified state, local simulation, selective archive and block streams implemented |
| Networks | Sepolia and Hoodi built in; other Glamsterdam networks through `skim run --network <dir>` (Cancun must be active by `amsterdamTime`); other subcommands are Sepolia diagnostics |
| Consensus presets | `mainnet` and `minimal`; preset-sized light-client verification and committee rotation |
| Language/toolchain | Rust 2024; Rust 1.97.0, set in `rust-toolchain.toml` |
| Core dependencies | reth v2.7.0 networking, libp2p/discv5, revm 43, alloy, redb, axum |

## 1. Thesis

An application can keep a small working set current without downloading the whole execution state or re-executing every block. skim applies the absolute post-values in EIP-7928 block access lists (BALs), resolves unknown state over SNAP with verified proofs, and executes calls locally.

Warm calls need no network state fetch. Cold calls depend on peer availability and the number of sequential dependencies: a proxy implementation, its code, then storage-derived addresses can require several rounds. A small resource footprint does not guarantee faster cold calls than a hosted node.

State, block data and light-client updates use P2P. Consensus bootstrap can obtain its initial trusted root from an HTTP checkpoint endpoint. Reference Ethereum JSON-RPC endpoints are used by diagnostic and differential-test tools, not as a state fallback in `skim run`.

## 2. Scope

Implemented workloads include local simulation and tracing, current state reads, signed-transaction broadcast and lookup, verified receipts/logs, WebSocket subscriptions, app block streams and selective state history.

The node does not reconstruct the whole state trie, re-execute every block, hold a general mempool or reconstruct arbitrary pre-Glamsterdam history. Default `latest` follows state-ready gossip optimistically; verified tags and `--require-verified` use the consensus light client (§8).

Example contract addresses and public-network measurements use Sepolia. A custom chain needs its own contract configuration and post-Glamsterdam EL/CL peers. `--hub` is reserved for a separate shared-data integration and currently refuses startup.

## 3. Protocol basis

### 3.1 EIP-7928: Block-Level Access Lists

BAL entries include account changes, storage changes and reads. Changes carry absolute post-values indexed by `block_access_index`: 0 for pre-execution system calls, 1 through n for transactions, and n+1 for post-execution work. Read-only entries carry no values and cannot establish a missing value.

`skim-bal` checks the commitment against the exact raw RLP bytes before decoding. `BlockWrites::from_bal` normalizes final per-key writes; `BlockWrites::before` builds changes preceding a transaction for mined-transaction replay.

### 3.2 Gloas ePBS execution-payload gossip (preferred fast path)

`skim-cl` receives `SignedExecutionPayloadEnvelope` through `execution_payload` gossip or consensus req/resp. It rebuilds the EL header and checks its block hash and BAL commitment. `skim run` does not check the payload against a signed beacon block or bid (only `skim probe-cl` reads the bid), and validates neither proposer nor builder signatures: a self-consistent envelope is accepted optimistically, and consensus authentication comes from the light client (§8). To limit what one forged envelope can do, a block below the head on another branch never rolls the chain back by itself (only a sibling of the head switches branches; deeper reorgs go through EL reconciliation against peers' chains). Only payloads that pass the commitment check are relayed; beacon blocks and attestations, which skim can't validate, are never relayed.

Networking uses discv5 and libp2p gossipsub, QUIC and TCP with yamux, and consensus Status/Metadata/Ping and payload/light-client request protocols. A configured network supplies genesis, preset, slots and fork versions. QUIC is dialed first when a peer announces it, TCP as the fallback. Blob-parameter fork digests follow `BLOB_SCHEDULE`; gossip topics overlap two epochs either side of a transition, and Status carries the digest in force. A live transition was crossed on the minimal-preset devnet. TCP peers that require a muxer other than yamux remain in [TODO](docs/TODO.md).

### 3.3 eth/71 (EIP-8159): BAL retrieval over EL P2P

Execution peers supply headers, bodies, receipts and `GetBlockAccessLists` by hash. Header links and commitments are verified before use. A valid empty BAL (`0xc0`) differs from unavailable data (`0x80`). EL BAL requests fill gaps, recover reorg branches, heal archived segments and backfill history while peers retain it. EIP-7928 requires BALs only for the weak subjectivity period (3,533 epochs, ~15.7 days); `skim probe-retention` measures what Sepolia peers actually serve. The EL fork id orders timestamp forks by activation, so forks scheduled after Amsterdam are announced correctly.

### 3.4 snap/2 (EIP-8189): state retrieval

skim supports snap/2 and its own snap/1 client for Geth peers. Account/storage point lookups use hashed address/slot keys; bytecode is checked against its code hash. A present value, proven absence, unavailable root, failed request and malformed answer are distinct outcomes.

Unproven empty answers are unavailable, never zero. A slot is zero only after an absence proof, a proven empty storage trie, or a complete verified trie rebuild. Peers generally serve recent roots; the resolver normally fetches at the parent root and replays verified BAL writes to the target.

## 4. Architecture

```text
CL payload gossip / req-resp --> payload verification --> chain placement --> MVCC store
EL headers + BALs -----------> gap/reorg recovery -----------^
EL SNAP + proofs -----------> scheduler --> verified bases + BAL replay --> store
store + headers ------------> revm simulator --> JSON-RPC / Rust library calls
verified block data --------> skim-chain --> receipts, logs, block streams, peer serving
CL checkpoint + updates ----> light client --> safe / finalized / require-verified
```

| Crate | Responsibility |
|---|---|
| `skim-bal` | BAL decoding, raw-byte commitment checks and normalized per-key writes |
| `skim-cl` | Consensus networking, SSZ, payload checks, network presets/digests and light-client primitives |
| `skim-el` | reth networking, configurable execution chain, snap/1 and snap/2, block requests, peer bookkeeping and serving |
| `skim-snap` | Adaptive SNAP scheduler, deduplication, hedging, priorities and Merkle-proof verification |
| `skim-state` | Per-key MVCC store, recent writes, rollback/GC, complete storage, restart persistence and selective archive |
| `skim-sim` | revm execution, state resolution, predictions, subscriptions, simulation sequences and tracers |
| `skim-chain` | Verified chain-data caches, transaction indexing/broadcast, log scans and the post-fork `history` database; `testkit` feature for dependent tests |
| `skim-cli` | `skim run`, chain placement, JSON-RPC/WebSocket, block streams, light-client orchestration, restore and diagnostic commands |

The Rust interfaces are the individual workspace crates. Chain placement lives in `skim-cli/src/chain.rs`; `skim-chain` owns `chaindata.rs` and `history.rs`. The [handoff](docs/HANDOFF.md#the-skim-hub-boundary) describes the shared-library boundary.

## 5. State model

### 5.1 Per-key MVCC

`StateKey` is `Balance(Address)`, `Nonce(Address)`, `CodeHash(Address)` or `Storage(Address, U256)`. Stored versions are ordered `(block number, U256)` values under the canonical block/hash chain. `StateValue::Unknown` is distinct from a known zero.

A base fetched at block H does not establish a value at H-1. In-flight fetches are committed with later indexed BAL writes replayed, so an old answer cannot overwrite a newer write. Reorgs explicitly roll back chain entries and versions above the common ancestor. The archive has a separate per-block rollback journal.

GC expires on-demand keys, retains the configured `--window`, and collapses older versions to one base. Unreferenced code is removed. A 257-entry canonical hash ring supports EVM `BLOCKHASH` independently of a shorter state window.

### 5.2 Recent BALs indexed by key

The store keeps write-only `BlockWrites` for the last `--window` blocks (default 256). Their sorted key vectors support lookup, promotion of recently written keys and base replay. Reads, touched addresses and new untracked code are stripped from this index. A local recent write is usable only if all intervening blocks are indexed.

This is separate from `skim-chain`'s 32-block decoded-BAL cache and 64-block raw-BAL serving cache. Memory depends on whole-chain writes, even for a tiny app watchlist.

### 5.3 Snapshot identity and views

A `Snapshot` holds an `Arc<Store>` and a `BlockRef` (number, hash, parent, state root). Reads check that the block is still canonical. A reorg or expiration can invalidate the view; holding a snapshot does not hold an independent MVCC generation beyond GC. Unknown keys can become known through verified fills at the same block.

Execution writes belong to revm's local state/overrides, not to the followed chain. Archived snapshots read only explicitly covered historical state and do not fetch missing old keys from SNAP.

### 5.4 Account existence

A BAL access alone proves neither existence nor a value. Proven absent accounts are represented by the appropriate zero balance/nonce and empty-code state. Complete-storage subscriptions verify the whole downloaded storage root before treating unlisted slots as zero.

## 6. Synchronization

### 6.1 Bootstrap

`skim run` loads the selected CL/EL network, opens restart persistence unless `--ephemeral`, starts both networks, and begins receiving payloads. Subscriptions resolve independently of background snapshot restore. The first applied block triggers recent-BAL seeding (up to 32 blocks), and its parent/ancestor hashes support later `BLOCKHASH` reads.

Restart data defaults to `~/.skim/<network>/restart.redb`. A saved snapshot is a cache: its headers and BALs must link to the current chain before its values are replayed. Unverifiable or over-8,192-block gaps are discarded. CL payload-range requests are tried for catch-up, with EL recovery available as a fallback.

### 6.2 Following

A verified envelope is decoded into writes and recent chain data. The chain placer accepts a continuation, rolls back a shallow fork, or fills a linked EL path to the new block. Store application produces `StateReady { block, changed_keys }`; RPC head notifications and subscription maintenance follow. Maintenance coalesces heads while a pass is running and then processes the newest one.

### 6.3 Resolving missing state

A read first uses held versions, verified complete storage or a contiguous recent BAL write. Otherwise, the resolver fetches at a usable recent ancestor root, verifies the answer, and replays BALs to the target. Unavailable data is retried within request/round limits, then refused with `-32002`.

### 6.3.1 State provider pool (trusted SNAP peers)

The scheduler uses connected SNAP-capable peers, scoring placement by latency and load with adaptive per-peer limits. It deduplicates jobs, batches code requests, hedges slow work (after the asked peer's own latency: mean plus four deviations, clamped to 150 ms..1.2 s, within a 10% hedge budget) and separates interactive/background priorities. `--trusted-peer` keeps selected EL peers connected; public discovery remains enabled in `skim run`. A trusted peer is not exempt from proofs, and keeping it connected does not guarantee availability. Saved-peer ranking favors useful stable peers at restart.

EL session defaults are 40 outbound and 15 inbound; CL peer target and mesh default to 50 and 8. These are separate from the scheduler's adaptive request concurrency.

### 6.4 Head semantics

- Default `latest`: newest state-ready selected block.
- `safe`: a sync-committee-supermajority-authenticated execution hash that matches the selected chain.
- `finalized`: the verified finalized execution hash, also matched by number and hash.
- `--require-verified`: uses the verified signed block for `latest`; refuses until available.
- `pending`: latest state, with sent-but-unincluded gap-free nonces added for `eth_getTransactionCount`; no general mempool.

## 7. Simulation

### 7.1 Native Rust API

The implemented library entry point is `skim_sim::Simulator`, constructed with a store, scheduler and predictor. Its methods include `call`, `call_retained`, `read`, `code`, `execute` and tracing/multi-call helpers. For example, after configuring a simulator and inserting the header for `block`:

```rust,ignore
use skim_sim::Mode;

let outcome = sim.call(&block, tx_env, Mode::Resolve { max_rounds: 8 }).await?;
let snapshot = sim.store.snapshot(&block)?;
let value = snapshot.get(&state_key)?;
```

`Mode::WarmOnly` refuses missing state without fetching. `Mode::Resolve { max_rounds }` predicts, resolves and retries. CPU-heavy execution is dispatched through the simulator's bounded blocking-work path; the EVM database itself performs no network I/O. `CallOutcome` records execution result, access set, fetched accounts/storage, resolve rounds and execution time.

### 7.2 Miss collection and cold-round minimization

A recording execution detects unknown accounts and slots. Its result is never returned while any dependency is missing. Actual misses and predicted keys share the next fetch wave; execution repeats until complete or its round budget is exhausted.

Predictions use exact calldata, generalized argument patterns, then frequent keys for a function shape. Learned sets have LRU entry limits and a global key budget. The optional Uniswap V3 predictor computes tick/bitmap reach and refreshes a TTL-bound background window. A prediction affects fetch timing, not correctness.

### 7.3 Execution environment

Calls use revm's Amsterdam rules and the selected execution chain id. A call at block H uses H's post-state and header environment. `eth_simulateV1` supplies sequential simulated blocks and overrides; `debug_traceCall` also accepts block/state overrides. Mined tracing uses the parent state plus BAL changes before each transaction and checks receipt status/gas before returning a trace.

### 7.4 JSON-RPC adapter

The adapter is in `skim-cli/src/rpc.rs`, served by axum over HTTP and WebSocket. Batch requests execute with up to eight concurrent handlers, fixing `latest` to one block. [The RPC reference](docs/guide/json-rpc.md) lists all implemented methods and compatibility limits.

Supported families include state reads, `eth_call`, gas estimation, `eth_simulateV1`, access lists/proofs, geth tracers, transactions, blocks, fee history and logs. Skim-specific methods are `skim_status`, `skim_dashboard`, `skim_subscriptionStatus`, `skim_archiveCoverage` and `skim_streamNext`.

Unavailable state/data returns `-32002`; explicit unsupported methods return `-32004`, unknown methods `-32601`, invalid parameters `-32602`, and execution reverts code `3`. WebSocket subscriptions support `newHeads`, `logs` and `skim_subscriptions`; HTTP filter creation is unsupported.

### 7.5 Product model: application-specific state client (M3)

Subscriptions declare accounts, storage slots or calls. They keep required keys current from BAL writes; readiness requires known values and bytecode at the reported block. Call subscriptions use `calls: [{to, data, from?}]`; `on_block: true` publishes fresh results after maintenance. Whole storage uses `storage: all` with a lifetime `max_slots` limit (50,000 by default).

On-demand keys stay tracked for `state.on_demand.cache_ttl_blocks` after use (default 4). The store enforces `max_subscribed_keys` (100,000) and `max_on_demand_keys` (200,000); both accept `unlimited`. Excess subscription keys stay on demand and a truncated subscription reports its missing coverage. There is no `skim_promote` RPC or `simulation`/`access.learned` YAML field.

See [configuration](docs/guide/configuration.md) for actual YAML. Chain selection is the `--network` flag, not a YAML field.

### 7.6 M4: selective state archiving

`retention: { mode: archive }` with `--archive <path>` records an initial base and changed versions for subscribed keys in redb. Coverage is per key and linked segment. Historical calls succeed only when every dependency is covered. Coverage is a set of intervals: unneeded archived keys are released after 64 blocks; old covered history remains readable, the blocks until tracking resumes are not covered, and resuming starts a new interval.

Restart gaps and `--archive-backfill N` use verified linked headers/BALs from peers. BALs contain post-values, so backfill cannot infer the value before the earliest recovered write. Unchanged keys can extend back only across a complete verified scan, including their known base boundary. There is no unlimited historical reconstruction guarantee.

After a restart, a reloaded archived key that an on-demand read warms first still gets its base archived.

`--history local` is a different database: it stores post-fork headers and fetched matching logs/coverage for streams and log queries. It does not archive arbitrary state or retain all historical BALs.

### 7.7 Transactions

`eth_getTransactionByHash`, `eth_getTransactionReceipt` and `debug_traceTransaction` find transactions through an in-memory index in `skim-chain`. `--tx-index subscribed` (the default) indexes only transactions whose BAL changes touch a subscribed address (one with any subscribed key); that includes the sender, through its nonce change. Transactions that only read a subscribed address can't be identified without re-execution and aren't indexed. `--tx-index all` indexes every transaction. `--tx-index-blocks N` sets the covered recent blocks (2,048 by default; 0 = none). Keys are 8-byte hash prefixes, confirmed against the transaction on lookup. Unindexed transactions return null.

`eth_sendRawTransaction` checks and gossips a signed transaction to EL peers. Sent transactions are always found, whatever `--tx-index` is: while pending they are re-broadcast every 12 s, and dropped after 30 minutes if never included; once included they are remembered for 24 h (at most 4,096). A reorg makes an included one pending again.

## 8. Trust model

All state fetches require proofs; code, body, receipt and BAL commitments are checked. Consensus authentication uses a sync-committee light client bootstrapped from a trusted weak-subjectivity checkpoint: explicit root, saved verified finalized root, then a configured HTTP endpoint. Only built-in Sepolia provides a default endpoint.

Light-client BLS updates and committee rotations support both presets and have been exercised end to end on the minimal-preset [devnet](devnet/README.md). Public peers may not serve the required data. Until verification succeeds, default gossip following remains available, while verified tags and `--require-verified` refuse unavailable verified blocks. Inspect `skim_status.light_client`.

skim does not independently validate every execution transition, proposer/attestation or full consensus fork choice. A state proof authenticates a value to a root; it does not authenticate an optimistic root to consensus. The static `web3_clientVersion` string still includes `unverified state`; use the explicit light-client status for verification state. See [trust and correctness](docs/guide/trust-and-correctness.md).

## 9. Benchmarks

Measurements identify the build, network, hardware, workload and cache state. The [P3 baseline](docs/experiments/p3-full-node-baseline.md) compared skim and Nethermind with Lighthouse at identical block hashes. Warm-throughput and correctness runs are in M5–M7; they are observations rather than universal performance guarantees.

Prometheus exports block arrival/head age, per-method requests/latency, peers and fetch outcomes, subscriptions, serving counters and process resources. `skim_memory_bytes`, `skim_memory_entries` and `skim_memory_capacity` report instrumented retained structures. These are estimates, not a heap profile: peer buffer bytes, some caches, allocator slack and code pages remain uncounted. Some capacity fields are collection capacities; others are configured limits or current counts.

The call cache clears before the next insert after reaching 20,000 entries or an estimated 32 MiB; a large single result can exceed the byte trigger. Block caches are count-bounded (32/64/128 blocks), the transaction index by `--tx-index-blocks` (2,048) and headers 8,192. Their fill times depend on slot duration, missed slots and historical fetches. Persistence/history/archive page-cache budgets are 8/16/64 MiB respectively.

## 10. Plan

M0–M7 delivered network feasibility, BAL following, verified state, the simulator and RPC, subscriptions, archive/backfill, generalized predictions, chain data, serving and regression gates. Experiment names are historical milestone labels, not a list of unimplemented APIs.

Further work is tracked in [TODO](docs/TODO.md): TCP peers without yamux, and whether RSS stays flat over 27 hours. `skim run` supports configured networks; `follow`, `probe-*` and the baseline workload remain Sepolia diagnostics. The [handoff](docs/HANDOFF.md) records test commands and the shared-library boundary.

## 11. Risks

- **Availability:** peers can prune state/BALs/receipts, disconnect or withhold data. Proofs protect integrity, not service.
- **Cold dependency depth:** unfamiliar calls can require several fetch rounds and wait for usable peers.
- **Consensus bootstrap:** a trusted root and peers serving usable light-client data are required for authenticated heads.
- **Reorgs and concurrency:** shared state, caches, restores and streams must remain on one hash-linked chain; regression tests cover the known races.
- **Memory:** count/byte-triggered caches do not define a total RSS cap. A nine-hour production run levelled off after ~7 h, but headers take ~27 h to fill and a 27-hour run is pending ([hardware](docs/guide/hardware.md#memory)).
- **Protocol changes:** new fork schemas or unsupported consensus presets need code support; a configured blob schedule alone does not implement arbitrary future forks.

## 12. Prior art

Helios authenticates state from RPC providers; full nodes execute and store the whole current chain state; archive nodes additionally retain arbitrary historical state. skim uses Ethereum peers, a selective working set and BAL replay. See the [comparison guide](docs/guide/compared.md) for operating tradeoffs and measurement sources.

## References

- [EIP-7928: Block-Level Access Lists](https://eips.ethereum.org/EIPS/eip-7928)
- [EIP-8159: eth/71 BAL exchange](https://eips.ethereum.org/EIPS/eip-8159)
- [EIP-8189: snap/2](https://eips.ethereum.org/EIPS/eip-8189)
- [EIP-7732: ePBS](https://eips.ethereum.org/EIPS/eip-7732)
- [EIP-1474: JSON-RPC error codes](https://eips.ethereum.org/EIPS/eip-1474)
- [Consensus specs](https://github.com/ethereum/consensus-specs)
- [devp2p](https://github.com/ethereum/devp2p)
- [reth](https://github.com/paradigmxyz/reth) and [revm](https://github.com/bluealloy/revm)
