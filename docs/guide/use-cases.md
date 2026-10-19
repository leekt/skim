# What to set up for your app

Find the row closest to what your app does, then read its section: what to configure, what to expect, and example apps. Most apps combine two or three of these. Latency and memory figures are measured on Sepolia unless marked otherwise; [memory and latency](memory-and-latency.md) explains the trade-offs behind them, and [hardware requirements](hardware.md) the footprint.

`skim run` also accepts configured Glamsterdam networks and both consensus presets ([networks](configuration.md#networks)); the bundled contract addresses and ~12 s/block timing estimates here are Sepolia-specific. Default `latest` is optimistic; use verified tags or `--require-verified` for consensus-authenticated reads ([trust](trust-and-correctness.md)).

| Your app needs | Set up | Expect |
|---|---|---|
| [Latest state of a few contracts](#1-latest-state-of-a-few-contracts) | `account` / `storage` subscriptions | reads in ms, current ~0.9 s after each slot |
| [The same view calls every block](#2-the-same-view-calls-every-block) | `calls` subscriptions with `on_block` | results pushed per block, no requests |
| [Many pinned contracts, plus reads beyond them](#3-many-pinned-contracts-plus-reads-beyond-them) | subscriptions + `storage: all` + predictors + on-demand cache | pinned reads in ms; the first unpinned read waits on peers |
| [Arbitrary calls on any contract](#4-arbitrary-calls-on-any-contract) | on-demand only (plus subscriptions for the popular ones) | warm in ms; cold reads take seconds, more on a fresh node |
| [Event history, indexed](#5-event-history-indexed) | `--history local` + `skim_streamNext` | logs from Glamsterdam on, reorg-safe, catch-up then the head |
| [State at past blocks](#6-state-at-past-blocks) | `retention: { mode: archive }` + `--archive` | any block since you subscribed (or ~2 weeks back with backfill) |
| [React to state changes](#7-react-to-state-changes) | `skim_streamNext` with `bal`, or WebSocket subscriptions | every change, per transaction, each block |
| [Send transactions and track them](#8-send-transactions-and-track-them) | `eth_sendRawTransaction` | gossiped to peers; your sent transactions tracked to inclusion and kept 24 h |
| [Simulate and trace](#9-simulate-and-trace) | the state your transactions touch, subscribed | `eth_call` / `debug_traceCall` / `eth_estimateGas` in ms on warm state |
| [Run on a small machine](#10-run-on-a-small-machine) | `--window 32`, few subscriptions | short macOS soaks measured ~100 MB median; long-run RSS grows until the caches fill |
| [Not served (yet)](#what-skim-doesnt-serve) | another tool | history before the fork, mainnet until it forks, whole-chain analytics, the mempool |

## 1. Latest state of a few contracts
**Need:** current balances, reserves, prices or flags of a handful of contracts, read often.

**Set up:** subscribe the accounts and slots you read:
```yaml
state:
  subscriptions:
    - name: vault
      address: "0x…"
      account: true                 # balance, nonce, code hash (and code)
      storage: ["0x0", "0x4"]       # the slots you read
```

**Expect:**
- Reads in milliseconds at any block of the in-memory window (`--window`, 256 blocks by default).
- The state is current about 0.9 s after each slot starts (median; 90th percentile ~1.8 s).
- Nothing fetched after the first read: each block's BAL keeps subscribed keys current.
- Memory: about 100 bytes per key, plus a version per change within the window.

**Examples:** a portfolio widget for known tokens, an oracle reader, a keeper watching one vault's health, a status page for a protocol's own contracts.

**Limits:** only what you subscribe is kept current. Reading other contracts falls into [4](#4-arbitrary-calls-on-any-contract).

## 2. The same view calls every block
**Need:** the result of a view call (a quote, a health factor, `getReserves`) at every block, without knowing which slots it reads.

**Set up:** a call subscription. skim runs it at each block, learns its access set, and subscribes that set:
```yaml
    - name: quote-1-weth
      calls: [{ to: "0xEd1f…2FB3", data: "0xc6a5…" }]
      on_block: true
```
Read results from `skim_subscriptionStatus`, or get them pushed over WebSocket with `eth_subscribe("skim_subscriptions")` ([websocket example](../../examples/websocket)).

**Expect:**
- A result per block, with no request from your side.
- `ready` tells you the whole access set is held.
- The call's access set counts toward `limits.max_subscribed_keys`. If it's cut short, the subscription reports `truncated` and isn't ready.

**Examples:** a price ticker (the [Uniswap V3 example](../../examples/uniswap-v3) keeps a 1 WETH quote warm), a liquidation monitor computing health factors, an ENS resolver kept warm (`resolve-warm` in [bigname](../../examples/bigname)).

**Limits:** a call that reads different slots each block (e.g. crossing ticks) changes its set. Keys it stops reading are released (for archived subscriptions, after 64 blocks).

## 3. Many pinned contracts, plus reads beyond them
**Need:** a set of hot contracts known in advance (pools, routers, tokens) answered instantly, plus whatever else a request happens to touch.

**Set up:**
- **Core contracts:** subscribe them. For a contract whose every slot may be read (a pool's ticks), use `storage: all` (downloaded once, verified against the storage root, then kept current; up to `max_slots`, default 50,000).
- **Ticks near the price:** the Uniswap V3 predictor (`predictors.uniswap_v3`) prefetches the tick range a quote can reach and keeps a window around the current tick warm.
- **Everything else:** the on-demand cache keeps it current for `cache_ttl_blocks` after use.
- **Finding what to pin:** the dashboard's "Addresses your calls read" (or `skim_dashboard.suggestion`) lists hot addresses not pinned yet, with a ready-made `skim.yaml` block.

**Expect:**
- Pinned reads in milliseconds.
- The first read of an unpinned account or slot waits for a SNAP peer: seconds when warm peers are connected. In the worst case measured, 40 cold calls on a fresh node took 6-53 s.
- After the first read, it's cached for `cache_ttl_blocks` (default 4) and repeats are milliseconds.
- `storage: all` costs ~100 bytes per slot.

**Examples:** DEX quoting and routing over a known set of pools ([Uniswap V3](../../examples/uniswap-v3), [Uniswap V4](../../examples/uniswap-v4)), an aggregator, a lending protocol's risk engine (markets pinned, user positions on demand).

**Limits:** cold reads depend on peers. On Sepolia, SNAP peers that serve the latest state are the scarce kind, so pin whatever must never wait.

## 4. Arbitrary calls on any contract
**Need:** reads you can't predict: whatever contract a user points at.

**Set up:** nothing beyond the defaults (`on_demand.enabled: true`). Subscribe the few contracts most requests hit (popular tokens), so only the long tail is cold. If the app also looks up arbitrary transactions by hash (a wallet's activity view, an explorer's transaction page), add `--tx-index all`: by default only transactions that change a subscribed address are indexed (see [8](#8-send-transactions-and-track-them)).

**Expect:**
- **Warm:** the first read fetches over SNAP with proofs, and verified values are cached for `cache_ttl_blocks`, so repeats answer in milliseconds.
- **Cold:** seconds. With no SNAP peer connected (common right after start), requests wait up to `--state-wait` (60 s by default), then return `-32002` if none arrives. A client may time out sooner. A provider fallback, if desired, belongs in your app; skim does not use one.
- 40 warm calls in one batch took 34-58 ms.

**Examples:** a wallet backend showing balances of arbitrary tokens, a block explorer's "read contract" tab, a generic contract UI.

**Limits:** peers serve state only for recent roots, so reads at older blocks of keys skim wasn't holding aren't possible (see [6](#6-state-at-past-blocks)). For a per-user, low-volume wallet, a light client in the wallet may fit better: see [skim compared](compared.md).

## 5. Event history, indexed
**Need:** every event of some contracts since some point, kept in your own index, correct across reorgs.

**Set up:**
- **`--history local`:** headers since Glamsterdam, plus the matches your streams asked for, are kept in the data dir.
- **Follow with `skim_streamNext`:** it delivers `apply` / `rewind` / `gap` events with a cursor. Store the cursor in the same transaction as your index changes ([JSON-RPC](json-rpc.md)).
- `eth_getLogs` works over the same history for ad-hoc queries.

**Expect:**
- **Reach:** logs from Glamsterdam (Sepolia block 11,856,337) at most. Before it: `-32004`.
- **Peers' retention:** receipts come from peers, so blocks older than what connected peers still serve are a `gap` unless skim already stored their matches. Measured on Sepolia on 2026-10-19 (`skim probe-retention`): all 7 eth/71 peers served receipts for every sampled block back to the fork block (~12.7 days).
- **Reorgs:** the stream rewinds; every response comes from one hash-linked range.
- **Catch-up:** catches up in batches, then follows the head, waking on each new block.
- **Disk:** grows with the matches you ask for, not with all logs. 140 MB for headers and smallname's matches on Sepolia.

**Examples:** [smallname](../../examples/smallname), an ENS API with its own SQLite index (names, owners, records, history); a token-transfer index for a wallet's activity feed; an NFT ownership index; a protocol's own analytics.

**Limits:** no history before the fork. To index from genesis you need an archive node or an indexer service. Lists can lag live reads by a block or two; [smallname's README](../../examples/smallname) explains how a UI should show that.

## 6. State at past blocks
**Need:** a value at block N, for N older than the in-memory window: balances for accounting, a pool's state for analytics, a quote at a past block.

**Set up:** archive retention on the subscriptions whose history you need, and an archive file:
```yaml
      retention: { mode: archive }
```
```
skim run --config skim.yaml --archive data/my-app.redb --archive-backfill 1000
```

**Expect:**
- Reads at any block since the key was subscribed, from disk.
- `--archive-backfill N` extends coverage up to ~2 weeks back (BAL retention). A key written within the backfilled range is covered from its first write. The value before that is unknowable from BALs, so it's refused, never guessed.
- Gaps after a restart are healed from peers' BALs.
- `skim_archiveCoverage([address])` summarizes where each archived key's coverage starts; it does not enumerate later gaps or release boundaries.

**Examples:** treasury accounting (balances at each month's end), TWAP or volume over your own pools, historical quotes for backtesting, proof-of-reserves snapshots.

**Limits:** only archived keys; no arbitrary state at arbitrary past blocks. Keys a call stops reading are released after 64 blocks: their coverage ends there, and reads past it are refused.

## 7. React to state changes
**Need:** to act when a balance or a slot changes, including changes no event announces (plain ETH transfers, internal calls).

**Set up:**
- **`skim_streamNext` with `bal: {addresses}`:** every change to those accounts from each block's access list, change by change, with the transaction index (`block_access_index`) and the kind (balance, nonce, code, storage). It works with or without log filters, and in the same response.
- **Or WebSocket:** `eth_subscribe("newHeads")`, `eth_subscribe("logs", filter)`, or `skim_subscriptions` for per-block subscription results.

**Expect:**
- Every change, because a BAL lists every write, not only the ones a contract chose to log.
- Delivered once the block's state is ready (~0.9 s median after the slot).
- Reorgs arrive as `rewind`.
- Blocks whose BAL is known but whose receipts expired still deliver their state changes (`logsAvailable: false`).

**Examples:** deposit detection for an exchange or a payment processor, alerting on a treasury's balance, a keeper that triggers on a vault's state, accounting that needs every ETH movement.

**Limits:** BALs for older blocks come from peers (~2 weeks retention) and aren't stored by `--history local` yet.

## 8. Send transactions and track them
**Need:** broadcast signed transactions and follow them to inclusion.

**Set up:** `eth_sendRawTransaction` ([send-transaction example](../../examples/send-transaction)).

**Expect:**
- The chain id, signature, nonce and balance are checked against verified head state.
- The transaction is gossiped to connected peers and re-broadcast every 12 s for up to 30 minutes.
- `eth_getTransactionReceipt` returns `null` until inclusion, then a receipt verified against the block.
- Transactions sent through skim are always found by hash: pending, then included (kept 24 h, at most 4,096), then expired. A reorg that drops the block makes them pending again, and they are re-broadcast.
- Other transactions are found by hash (`eth_getTransactionByHash`, `eth_getTransactionReceipt`, `debug_traceTransaction`) only if indexed: by default (`--tx-index pinned`) those whose state changes touch a subscribed address, which covers ones it sent (its nonce changes) and ETH it received; a transaction that only reads a subscribed contract isn't indexed. `--tx-index all` indexes every transaction. Either way the index covers the last `--tx-index-blocks` blocks (2,048, ~7 h); anything else returns `null`.

**Examples:** a wallet's send flow, a relayer, a bot submitting transactions.

**Limits:** no mempool view (skim doesn't follow pending transactions), and no lookup by hash of transactions older than the index window, except ones sent through skim (kept 24 h).

## 9. Simulate and trace
**Need:** run a transaction before sending it: preview, gas estimate, access list, trace.

**Set up:** `eth_call`, `eth_estimateGas`, `eth_createAccessList`, `eth_simulateV1`, `debug_traceCall` (geth tracers), with state overrides. Subscribe the contracts your transactions usually touch.

**Expect:**
- Milliseconds on warm state: ~21,000 `eth_call`/s at 100 in flight on the development machine.
- Unpinned state is fetched as in [3](#3-many-pinned-contracts-plus-reads-beyond-them).
- Results were checked against reference nodes for every tracer, state overrides and `eth_simulateV1`.

**Examples:** a wallet's transaction preview and gas estimate, an intent solver evaluating routes, a searcher simulating bundles against the latest block.

**Limits:** simulation runs on verified state at a block skim holds; a cold access in the middle of a simulation waits for peers.

## 10. Run on a small machine
**Need:** the smallest footprint: a cheap VPS, a home box, a sidecar.

**Set up:** `--window 32`, subscriptions only for what you read every block, defaults otherwise. `--ephemeral` if you can't keep a data dir (restarts then start empty).

**Expect:**
- ~100 MB RSS following the head (macOS soak median 99-105 MB).
- 0.25-0.5 core, and ~0.3 MB/s in plus ~0.1 MB/s out of steady traffic.
- Memory rises over the first hours as block caches fill, then levels off ([hardware](hardware.md#memory) has the long-run measurements; [memory and latency](memory-and-latency.md#choosing) the knobs).

**Examples:** a personal RPC for your own wallet, a sidecar next to a bot, a small dashboard.

**Limits:** reads at older recent blocks and deep reorgs need a fetch (or the archive). Cold reads still depend on peers.

## What skim doesn't serve
- **History before Glamsterdam:** logs below the known fork floor return `-32004`; missing historical state returns `-32002`. Use an archive node or an indexer for genesis-to-now history.
- **Pre-Glamsterdam networks:** skim needs BALs and Gloas payload envelopes. Sepolia is built in; other compatible networks use `--network <dir>`. There is no built-in `--network mainnet` shortcut.
- **Whole-chain analytics:** skim keeps what you use, not the chain. Queries across every contract and every block need a full or archive node and a data warehouse.
- **The mempool:** pending transactions aren't followed.
- **Old transactions by hash:** the index covers the last 2,048 blocks (~7 h) by default, in memory; with the default `--tx-index pinned`, only transactions touching subscribed addresses.
