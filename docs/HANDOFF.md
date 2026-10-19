# Handoff: skim

For an agent picking up work on **skim** (this repo). The sibling project **skim-hub** (a separate, private repo) has its own handoff. skim must stay standalone and trust-minimized; see [The skim-hub boundary](#the-skim-hub-boundary) for what is shared and what is being built where.

Status as of 2026-10-19, `main` at `3e2c75d`: through the pinned transaction index (`9ed1f49`, `c774387`), `probe-retention` (`eac7020`), the EL fork-id fix (`a1ac222`), the archive base fix (`e13b916`) and the devnet state edge cases (`f87bca2`).

## What skim is
A minimal Ethereum node for apps: it follows payloads over CL gossip, checks BAL/header commitments, keeps subscribed and TTL-bound on-demand state (multi-version for `--window` blocks), fetches unknown state from peers over SNAP with proofs, and serves JSON-RPC, WebSocket subscriptions, `skim_streamNext` block streams and a selective archive. Default `latest` is optimistic; `safe`, `finalized` and `--require-verified` require light-client-authenticated execution hashes on the selected chain.

**It depends on Glamsterdam** (BALs, eth/71, ePBS payload envelopes), so it runs on networks past that fork: Sepolia today, and any other via `--network <dir>`.

Read first:
- [README](../README.md)
- [how it works](guide/how-it-works.md)
- [configuration](guide/configuration.md)
- [what to set up for your app](guide/use-cases.md)
- [memory and latency](guide/memory-and-latency.md)
- the experiment logs in [docs/experiments/](experiments), M7 especially: every measurement, review and soak is recorded there

## Layout
| Crate | What |
|---|---|
| `skim-el` | EL p2p on reth's network stack: sessions, requests (headers, bodies, receipts, BALs, SNAP), serving peers, `ElNetwork` (chain spec, bootnodes) |
| `skim-cl` | CL p2p (libp2p gossip, req/resp), SSZ, payload verification, light client, `network` (presets, network config, fork digest) |
| `skim-bal` | BAL decoding and normalization (`BlockWrites`) |
| `skim-state` | the multi-version store, recent-BAL index, archive (`redb`), restart snapshot |
| `skim-snap` | SNAP scheduler (AIMD per peer, hedging, dedup) |
| `skim-sim` | EVM (revm) over the store, subscriptions, predictors, prefetch |
| `skim-chain` | chain data (`chaindata`: headers, bodies, receipts, BALs from peers, verified; log scans; the transaction index, lookup and sending) and the post-fork history store (`history`). A library, shared with skim-hub; `testkit` feature for dependent crates' tests |
| `skim-cli` | the `skim` binary: `run` (the node), JSON-RPC (`rpc.rs`), streams (`stream.rs`), chain placement (`chain.rs`), restart persistence, probes |
| `examples/` | viem examples, `smallname` (an ENS API with its own SQLite index, the reference indexer), `bigname`, `devnet-edgecases.mjs` |
| `devnet/` | a local Glamsterdam devnet (`run.sh`) and its differential check (`check.py`) |
| `metrics/` | Prometheus + Grafana stack, `gen_dashboard.py` generates the dashboard |

## How to work here
- **Tests:** `cargo test --profile ci --workspace --locked` (188 tests). smallname (7 tests): `npm ci` then `npm run -s smallname-test` in `examples/` (CI uses Node 22.23.2). Fuzz regression replay is a separate nightly CI job; see [fuzzing](../fuzz/README.md).
- **Every fix has a regression test that fails without it.** Check by reverting the fix (a mutation check); the reviews in M7 record that each was checked.
- **Reviews:** external reviews arrive as findings (P1/P2). Record each in M7 as a table (finding, fix, regression test) and say which were mutation-checked.
- **Open work:** [`docs/TODO.md`](TODO.md). Remove an item when done, and say so in the commit.
- **Docs are part of the change:** update the guides and the README when behaviour changes. The docs use measured numbers and say what is estimated or unmeasured.
- **Commits:** end messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Push to `main`; CI must pass. Hub-mode work goes on the `hub-mode` branch, not `main` (see below).
- **Secrets:** RPC keys and test keys live in `.env` (gitignored). Never print or commit them; redact URLs in any output you share.
- **Deployment details** (servers, addresses, tunnels) never go in this repo, which is public. A production deployment exists; ask the owner.
- **Deleting files:** use `trash`, not `rm -rf` (a hook blocks it).
- **Soaks:** `docs/experiments/tools/chaos_soak.py` (150 min; needs `REF_URL` from `.env`). The reference provider has been rate-limiting (HTTP 429). Check that the run's `ref_unavailable` count is low before trusting its correctness numbers.
- **Memory A/B:** `docs/experiments/tools/mem_ab.py` (several builds, same load, side by side).

## Where things stand
**Any Glamsterdam network by config** (`ec5ced9` and after):
- **The flag:** `--network <sepolia|dir>` reads a standard network-config directory: `config.yaml` with `PRESET_BASE` (`mainnet` or `minimal`), `genesis.ssz` or `genesis_validators_root.txt`, `bootstrap_nodes.txt`, plus the EL `genesis.json` (with `amsterdamTime`) and optional `enodes.txt`. A custom network is refused unless Cancun (EIP-6780) is active by `amsterdamTime` (`a1ac222`).
- **Presets:** `mainnet` and `minimal` (`skim_cl::network::Preset`); the light client follows the preset (`LcConfig`). Minimal-preset light-client tests are in (`6fd23e3`).
- **Fork digest:** computed (Fulu+: fork data root XOR the blob-parameter hash); Sepolia's matches the digest its peers use. It follows scheduled blob-parameter forks while running (`25c2d2e`), subscribes to the next topics two epochs early and retires the previous topics two epochs later (`0fcf9d1`). Status carries the digest in force (`162eb48`).
- **EL fork id:** timestamp forks are ordered by activation, so forks scheduled after Amsterdam give the right fork id (`a1ac222`; reth had ordered them in mainnet order).
- **Discovery:** QUIC-only peers are discovered and dialed. QUIC is tried first when announced, TCP as the fallback (`82131c4`).
- **Process-wide network and chain id:** `skim_cl::network::current()` and `skim_sim::env::chain_id()`, Sepolia by default.
- **Flags:** `--el-bootnode`, `--cl-peer` (failures are logged), `--hub` (refuses to start: "not supported yet").
- **Command scope:** only `run` takes `--network`. `probe-*` (including `probe-retention`), `follow` and `bench-node` remain Sepolia diagnostics; their help says so (`959adcf`).

**On the devnet** ([devnet/](../devnet), `devnet/run.sh`): verified end to end on the minimal preset, with peers from discovery alone. The light client verifies and rotates, blocks apply ~0.53 s after their slot, and answers match the devnet's EL node (`devnet/check.py`). A live blob-parameter fork was crossed without losing peers (TODO 24 closed, M7). State edge cases (`examples/devnet-edgecases.mjs`: creation, selfdestruct, slot clearing, EIP-7702, a reverted call with value) match Nethermind at each block hash, all 11 steps (`f87bca2`).

**Transaction index** (`9ed1f49`, `c774387`): `--tx-index pinned|all` (default `pinned`: only transactions whose BAL changes touch a pinned address, i.e. one with any subscribed key; that covers the sender through its nonce change; read-only interactions aren't indexed) and `--tx-index-blocks N` (2,048 by default, 0 = none). Keys are hash prefixes, confirmed against the transaction. Transactions sent through skim are always found: pending, then included (kept 24 h, at most 4,096), then expired; a reorg makes them pending again. `eth_getTransactionByHash`, `eth_getTransactionReceipt` and `debug_traceTransaction` return null for unindexed transactions.

**BAL retention on Sepolia** (`probe-retention`, 2026-10-19): all 7 eth/71 peers served BAL, body and receipts for every sampled block back to the fork block 11,856,337 (~12.7 days). EIP-7928 only requires BALs for the weak subjectivity period (3,533 epochs, ~15.7 days). One Geth peer had expired pre-merge history (M7).

**Archive:** after a restart, a reloaded archived key warmed by an on-demand read first still gets its base archived (`e13b916`).

**smallname:** a live read skim couldn't answer is `read_status: failed` (`state_unavailable`), never null or absent; a revert still means absent (`9e3cd0a`). A reorg restores registry placements (`mount_undo`, `2e70c07`). The page shows "values read at block X" beside "discovery indexed through Y".

**Reviews:** 13 and 14 are fixed and recorded in M7.

**`skim-chain` extracted** from `skim-cli` (`c225129`): chain data and the history store as a library (no behaviour change; `ChainData::new` takes the chain id). skim-hub can depend on it.

**Memory:** nine production hours (Linux aarch64, `--history local` plus an indexer, on the build that indexed every transaction) are in [hardware](guide/hardware.md#memory): RSS 147 MB at start, 346 MB at 9 h, flat after ~7 h; 305 of 339 MB was mimalloc's heap. A run on the `--tx-index pinned` build started 2026-10-19 11:31 UTC; the 27 h result decides TODO 6 and the README's footprint row, which is deliberately left until then. The [memory audit](experiments/memory-audit.md) (2026-10-18) preserves the earlier investigation; its proposals have been implemented.

## Next tasks (in order)
1. **Open items in [`docs/TODO.md`](TODO.md):**
   - 6: memory past 27 h: read the pinned-index run when it reaches 27 h, update [hardware](guide/hardware.md#memory), [memory and latency](guide/memory-and-latency.md) and the README footprint row, then close or narrow the item;
   - 25: TCP peers without yamux.
2. **Pending measurements:**
   - **Huge pages on/off for speed:** a `skim bench-node` A/B on Linux.
   - **Catch-up speed** (blocks/s during a stream backfill) for the use-case guide.
   - **A soak with full correctness coverage,** once the reference provider stops rate-limiting.
3. **Ideas discussed, not started:**
   - per-client quotas on block streams;
   - subscriptions registered per client over RPC, under one budget.

## The skim-hub boundary
The owner's decision, 2026-10:
- **skim-hub** stays a separate, private repo: a shared chain-data hub for several skim instances, and a **trusted** setup. It keeps the pre-Glamsterdam reconstruction, which skim can't verify.
- **The verifiable part becomes a skim mode:** post-fork headers, BALs and receipts, served with their provenance. It is being built on the `hub-mode` branch, **not on `main`**. Docs on `main` must not describe hub mode as available until it is merged.
- **The leaf side** (`--hub <url>`) stays refused on `main` until the owner asks for it. When implemented, it must be opt-in, record the provenance of every hub answer, keep the free checks (header commitments) on, and fall back to public peers.
- **What skim provides skim-hub:** crates (`skim-el`, `skim-cl`, `skim-bal`, `skim-state`, `skim-chain`), consumed as git dependencies pinned to a commit. Keep their public APIs stable, or coordinate changes.
