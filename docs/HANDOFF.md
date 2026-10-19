# Handoff: skim

For an agent picking up work on **skim** (this repo). The sibling project **skim-hub** (a separate, private repo) has its own handoff. The two are developed separately: skim must stay standalone and trust-minimized, and the hub only ever reaches skim through crates and a versioned API (see [The skim-hub boundary](#the-skim-hub-boundary)).

Status as of 2026-10-19, `main` at `ec5ced9` or later.

## What skim is
A minimal Ethereum node for apps: it follows the chain over p2p (EL and CL gossip), verifies each block against its BAL commitment and the light client, keeps only the state an app subscribes to (in memory, multi-version for `--window` blocks), fetches the rest from peers over SNAP with proofs, and serves JSON-RPC plus skim-specific methods (subscriptions, `skim_streamNext` block streams, archive).

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
| `skim-cli` | the `skim` binary: `run` (the node), JSON-RPC, chain data and history (`chaindata.rs`, `history.rs`), streams (`stream.rs`), probes |
| `examples/` | viem examples, `smallname` (an ENS API with its own SQLite index, the reference indexer), `bigname` |
| `metrics/` | Prometheus + Grafana stack, `gen_dashboard.py` generates the dashboard |

## How to work here
- **Tests:** `cargo test --profile ci --workspace` (172+ tests). smallname: `npm run -s smallname-test` in `examples/`.
- **Every fix has a regression test that fails without it.** Check by reverting the fix (a mutation check); the reviews in M7 record that each was checked.
- **Reviews:** external reviews arrive as findings (P1/P2). Record each in M7 as a table (finding, fix, regression test) and say which were mutation-checked.
- **Open work:** [`docs/TODO.md`](TODO.md). Remove an item when done, and say so in the commit.
- **Docs are part of the change:** update the guides and the README when behaviour changes. The docs use measured numbers and say what is estimated or unmeasured.
- **Commits:** end messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Push to `main`; CI must pass.
- **Secrets:** RPC keys and test keys live in `.env` (gitignored). Never print or commit them; redact URLs in any output you share.
- **Deployment details** (servers, addresses, tunnels) never go in this repo, which is public. A production deployment exists; ask the owner.
- **Deleting files:** use `trash`, not `rm -rf` (a hook blocks it).
- **Soaks:** `docs/experiments/tools/chaos_soak.py` (150 min; needs `REF_URL` from `.env`). The reference provider has been rate-limiting (HTTP 429). Check that the run's `ref_unavailable` count is low before trusting its correctness numbers.
- **Memory A/B:** `docs/experiments/tools/mem_ab.py` (several builds, same load, side by side).

## Where things stand
**Just landed: any Glamsterdam network by config** (`ec5ced9`).
- **The flag:** `--network <sepolia|dir>` reads a standard network-config directory: `config.yaml` with `PRESET_BASE`, `genesis.ssz` or `genesis_validators_root.txt`, `bootstrap_nodes.txt`, plus the EL `genesis.json` (with `amsterdamTime`) and optional `enodes.txt`.
- **Presets:** `mainnet` and `minimal` (`skim_cl::network::Preset`). The light client uses `LcConfig`, so committee size and period length follow the preset.
- **Fork digest:** computed (Fulu+: fork data root XOR the blob-parameter hash). A test checks that Sepolia's published config gives `0x669e6c11`, the digest Sepolia peers use.
- **Process-wide network and chain id:** `skim_cl::network::current()` and `skim_sim::env::chain_id()`, set once at startup; Sepolia by default, which keeps tests unchanged.
- **New flags:** `--el-bootnode`, `--cl-peer`, and `--hub` (refuses to start: skim-hub isn't supported yet).
- **Unchanged:** the probe commands (`probe-*`, `follow`) are still Sepolia-only.

**Tried against a local Kurtosis devnet** (minimal preset, Gloas at epoch 4; config in the skim-hub repo, `devnet/network_params.yaml`):
- The network config loads (chain id 3151908, minimal preset, 6 s slots).
- EL sessions come up with geth and Nethermind.
- **CL peers don't connect from the host:** Lodestar advertises QUIC only (UDP 9001), Kurtosis doesn't map that UDP port, and skim's direct TCP dials (`--cl-peer`) produced no connection and no error log. So skim never got past "following" to applied blocks there.

## Next tasks (in order)
1. **Follow the devnet end to end.**
   - Make CL connectivity work: most likely run skim as a container inside the Kurtosis network (the release workflow builds an image), or map the beacon nodes' TCP and QUIC ports and make sure the explicit dial path logs its failures.
   - Done when skim applies devnet blocks, its light client bootstraps from the devnet's finalized root, and `eth_call` matches a devnet node.
2. **Light-client tests for the minimal preset:** the testkit builds mainnet-size committees (512). Add a 32-member variant, and run the bootstrap/update tests under both presets.
3. **BPO forks after start:** the fork digest is computed once at startup. A blob-parameter fork that activates while skim runs changes the digest, so topics must be resubscribed. Done when a test crosses a `BLOB_SCHEDULE` epoch.
4. **Extract a `skim-chain` library** from `skim-cli`: chain following and the history store (`ChainData`, `HeaderStore`), with no behaviour change. skim-hub will depend on it.
5. **Pending measurements:**
   - **Long-run memory:** whether RSS levels off once every cache is full (the transaction index fills over ~7 h, headers over ~27 h). Watch `skim_memory_bytes{part}` against RSS in Grafana; update [hardware](guide/hardware.md) and [memory and latency](guide/memory-and-latency.md), which state it as open.
   - **Huge pages on/off for speed:** a `skim bench-node` A/B on Linux.
   - **Catch-up speed** (blocks/s during a stream backfill) for the use-case guide.
   - **A soak with full correctness coverage,** once the reference provider stops rate-limiting.
6. **Ideas discussed, not started:**
   - per-client quotas on block streams, so one indexer's backfill can't starve others;
   - subscriptions registered per client over RPC, under one budget.

Loose end: `memory_usage.md` (repo root, uncommitted) is a memory-investigation handoff to another agent. The owner decides whether it gets committed.

## The skim-hub boundary
- **skim-hub** is a separate, private repo: a shared chain-data hub for several skim instances. It's a **trusted** setup; data from before Glamsterdam can't be verified by skim.
- **What skim provides it:** crates (`skim-el`, `skim-cl`, `skim-bal`, `skim-state`, and `skim-chain` once extracted), consumed as git dependencies pinned to a commit. Keep their public APIs stable, or coordinate changes.
- **The leaf side** (`--hub`) lives in this repo, but stays refused until the hub's API is specified (in the skim-hub repo) and the owner asks for it. When implemented, it must be opt-in, record the provenance of every hub answer, keep the free checks (header commitments) on, and fall back to public peers.
