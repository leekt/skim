# JSON-RPC reference

skim serves standard Ethereum JSON-RPC over HTTP and WebSocket on the same address (`--rpc`, default `127.0.0.1:8545`; `ws://` for WebSocket), with single and batch requests. Unmodified viem, ethers, web3.py and Foundry (`cast`, `forge script --rpc-url`) work against it.

`eth_chainId` and `net_version` use the execution genesis selected by `--network`. The Sepolia block numbers and contract examples below apply to the built-in network; see [network configuration](configuration.md#networks) for custom Glamsterdam chains.

## Methods
| Method | Notes |
|---|---|
| `eth_call` | Local revm. Unknown state is fetched (verified) first. Honours `gasPrice` / `maxFeePerGas` / `maxPriorityFeePerGas` (0 if omitted, as nodes do), `accessList`, `authorizationList` (EIP-7702) **state overrides** (`balance`, `nonce`, `code`, `state`, `stateDiff`) and **block overrides** (parameter 3, as for `eth_simulateV1`). Blob fields are rejected (`-32602`), never ignored. Gas is capped at the block's gas limit (at least 30M). |
| `eth_simulateV1` | Sequences of calls across simulated blocks (each call sees earlier calls' writes), with per-block `blockOverrides` (number, time, gas limit, fee recipient, base fee, prevrandao, `blobBaseFee`, `blockHash`; `beaconRoot` is refused, as simulated blocks run no system calls) and `stateOverrides`. `BLOCKHASH` of a block after the base block (a simulated one) answers a stand-in hash derived from its number. Limits: 256 blocks, 1,000 calls, and a gas budget of four times the per-call cap over all calls (`-38026`); each call's gas is capped as for `eth_call`, whatever `blockOverrides.gasLimit` says. With `validation: true`, calls that together exceed the block's gas limit fail with `-38015`. Per call: `status`, `returnData`, `gasUsed`, `logs`, `error`. `validation: true` checks nonces, balances and EIP-1559 base fees (simulated blocks get proper base fees) with the spec's error codes (e.g. `-38012`). `traceTransfers`: on Glamsterdam, ETH transfers already emit native EIP-7708 logs, so these are what you get. |
| `debug_traceCall` | geth tracers: default (struct logs), `callTracer`, `prestateTracer`, `4byteTracer`, `muxTracer`, `noopTracer`, with `stateOverrides` and `blockOverrides`. Not yet: `flatCallTracer`, JS tracers. |
| `debug_traceTransaction`, `debug_traceBlockByNumber`, `debug_traceBlockByHash` | Replays mined transactions with the same tracers. Each runs on its exact pre-state: the parent block's state plus the block's access-list changes before it (no earlier transaction is re-executed). Every replay must reproduce the transaction's receipt (status and gas, verified against `receiptsRoot`), or the trace is refused. For blocks inside `--window`. `debug_traceTransaction` finds the transaction with the same lookup as `eth_getTransactionByHash`; an unindexed one gets `-32002` (reason `not_indexed`). |
| `eth_createAccessList` | geth's algorithm (iterate until the list is stable; gas is measured with the final list). |
| `eth_getProof` | EIP-1186 proofs assembled from SNAP answers and verified against the block's state root before they're returned. Only for recent blocks, because peers serve only recent state roots. |
| `eth_feeHistory` | Base fees (including the next block's) and gas-used ratios from headers; reward percentiles with geth's algorithm, from verified bodies and receipts. Blob fee fields are omitted; the CL blob schedule used for gossip fork digests does not add these RPC fields. |
| `eth_getBlockReceipts`, `eth_getTransactionByBlock{Number,Hash}AndIndex`, `eth_getBlockTransactionCountBy{Number,Hash}`, `eth_getUncle*` | From verified bodies and receipts; uncles are always empty. |
| `eth_estimateGas` | Binary search on local execution, once the first run made the state local, within the `eth_call` gas cap. State and block overrides are refused (`-32602`), not ignored. |
| `eth_getBalance`, `eth_getCode`, `eth_getStorageAt` | Tracked: local. Untracked: one verified fetch. |
| `eth_getTransactionCount` | `pending` adds transactions sent through this skim and not yet included (gap-free nonces only). |
| `eth_sendRawTransaction` | Checks chain id, signature, `nonce too low`, and `insufficient funds` against verified head state. Then gossips to all connected EL peers, re-broadcasting every 12 s until included (gives up after 30 min). Blob transactions are rejected. |
| `eth_getTransactionByHash` | Transactions sent through skim, and transactions indexed in the last `--tx-index-blocks` (2,048) blocks. By default (`--tx-index subscribed`) a transaction is indexed only if its block's access list shows it changing a subscribed address (an address with any subscribed key); that covers transactions sent from one, since the sender's nonce changes. A transaction that only reads a subscribed contract isn't indexed (BAL reads carry no transaction position). `--tx-index all` indexes every transaction. Unindexed transactions return `null`. Index entries are 8-byte hash prefixes; each hit is confirmed against the block's verified transactions. Sent transactions are always found: pending (with no block fields) until a block includes them, then included (remembered for 24 h, at most 4,096, even after the index window passes them); a reorg that drops the block makes them pending again. A sent transaction never included is dropped after 30 min. See [`--tx-index`](configuration.md#skim-run-flags). |
| `eth_getTransactionReceipt` | Same lookup as `eth_getTransactionByHash`. `null` while pending, or for a transaction that isn't indexed. Then built from the block's receipts, fetched from peers and checked against `receiptsRoot`. |
| `eth_getLogs` | Address / topics / `fromBlock`–`toBlock` / `blockHash`; `safe` and `finalized` resolve through the light client as elsewhere. A `toBlock` past the head is clamped to it; a `fromBlock` past the head returns `[]` (nothing yet), so polling `last + 1 ..= latest` never repeats a block. Range ≤ `--max-logs-range` (2,048). Only blocks whose bloom (and, for followed blocks, BAL) can match have their receipts fetched; receipts are verified. History depends on `--history`. With `off` (the default, head-only), logs cover the in-memory window, about the last day. With `local`, history reaches back to Glamsterdam (Sepolia block 11,856,337) at most: headers since the fork are kept in `history.redb` in the data dir, and so are block streams' matches: per block, which filter atoms (one address with its topic constraints, or topic constraints alone) its receipts were checked against and the logs matching them. Coverage only grows, so a module whose filters grow (smallname, as ENSv2 contracts are discovered) keeps what earlier scans stored, and the store grows with what modules ask for, not with the chain. Headers are walked back hash by hash from peers on first use. Receipts come from peers, so a range also has to be within what connected peers still serve: each announces its oldest block (eth/69 `earliest_block`), and older ranges get `-32002`. `skim_status.history.logs_from` says where logs are available now. Blocks before the fork get `-32004`. With `--ephemeral`, about the last day only. |
| `eth_getBlockByNumber`, `eth_getBlockByHash` | Header fields plus transaction hashes, or full transaction objects with `true`. Bodies are verified against `transactionsRoot`. |
| `eth_blockNumber` | The latest **state-ready** block. |
| `eth_chainId`, `net_version`, `net_listening`, `eth_syncing`, `web3_clientVersion` | |
| `eth_gasPrice`, `eth_maxPriorityFeePerGas` | Head base fee + 1 gwei tip, and the 1 gwei tip. |
| `skim_dashboard` | One document with peers by client (with SNAP latency), block arrival, per-method RPC latency, the addresses `eth_call` read (subscribed or not, keys fetched), and a suggested `skim.yaml` block. For history, use the Prometheus metrics (`--metrics`). |
| `skim_subscriptionStatus` | Per subscription: `ready`, `block`, `required_keys`, `missing_keys`, `missing` (up to 8 of the missing keys, named), `truncated`, `calls_ok`, `error`, `results` (for `on_block` calls). |
| `skim_status` | Head, store counts (subscribed/on-demand keys, bytecode, refused admissions), peer acquisition and request counts, light-client status, history availability, `network` (`name`, `glamsterdam_at`, and `forks`: `hegota {at, support}` and `stops_at {fork, at, reached}`, where skim stops: [Hegota](hegota.md)), serving counters, and `memory.parts`: per-structure entries, estimated retained payload bytes and reported capacity. These estimates do not account for all heap/RSS. |
| `skim_archiveCoverage` | Parameter `[address]`; returns tracked archived keys for that address with `key`, `from` and `starts_at_write`. This is a coverage-start summary, not a full list of segment boundaries or a guarantee that every subsequent block is covered. |
| `skim_streamNext` | A block stream for app modules (indexers): follow the chain through skim instead of re-implementing catch-up, retries and reorgs. Request `{cursor, recent, fromBlock, filters, bal, maxBlocks, waitMs}`; response `{events, cursor, nextFrom, head, finalized, logsFrom}`. Events, in order: `rewind {to}` (undo everything above `to`: the newest of your `recent` blocks still on the chain), `gap {from, to, scope, reason}` (data skim can no longer serve: `scope: "logs"` when receipts expired, `"all"` when skim has no headers there), `apply {block, logs, stateChanges?, logsAvailable?}` (blocks with logs matching any filter, or, with `bal: {addresses}`, changes to those accounts from the block's access list, change by change with its `block_access_index`, kind (`balance`, `nonce`, `code`, `storage`), slot and value; others are skipped, and `cursor` says how far the scan went). Logs and state changes are separate channels: a block in a logs gap still gets an `apply` for its state changes, with `logsAvailable: false`. Everything in one response (logs, state changes, `cursor`) comes from one hash-linked range of blocks continuing your cursor. With `waitMs`, a request at the head returns as soon as skim applies the next block. State changes come from BALs skim has or peers serve (recent blocks); they aren't stored by `--history local` yet. Pull-based: skim keeps no per-module state or queue. Store `cursor` in the same transaction as your index changes, and a crash re-delivers instead of skipping. `finalized` is the light client's verified finalized block, for exports that can't be undone. See [`examples/smallname`](../../examples/smallname). |

## WebSocket subscriptions
Every method works over WebSocket too. In addition, `eth_subscribe` / `eth_unsubscribe` support:

| Subscription | Pushes |
|---|---|
| `newHeads` | the header of each block whose state is ready, including every block of a gap fill or a new branch after a reorg |
| `logs` (with an `address` / `topics` filter) | each matching log of each new block, one notification per log. When a reorg removes a block whose logs were pushed, those logs are pushed again with `removed: true` (newest first), then the new branch's logs, as geth does |
| `skim_subscriptions` | after each block: every subscription's `ready`, `missing_keys`, `calls_ok`, `error` and latest `on_block` `results` |

Each subscription starts at the head current when it was made and reports every block after it, up to the last 64 blocks it missed (a gap fill, or a slow reader the broadcast left behind), when the next block arrives. A `logs` subscription moves on only once a block's logs were delivered: a block whose receipts can't be fetched is tried again on the next block. If it can't be told everything (more than 64 blocks behind, or receipts failing 8 blocks in a row), it ends with an error notification (`params.error`, code `-32002`) rather than skipping blocks silently; subscribe again. With `--require-verified`, `newHeads` and `logs` follow the verified head, like every read. Limits: 128 subscriptions per connection (`-32005`) and 1,000 requests per batch, HTTP or WebSocket (`-32600`).

viem's `watch*` helpers use these automatically with `webSocket(...)`.

## Block parameters
| Tag | Meaning |
|---|---|
| `latest` (or omitted) | The latest state-ready block; with `--require-verified`, the highest signed block available on the selected chain. Fixed for a whole batch request. |
| a number, or `{blockNumber}` / `{blockHash}` (EIP-1898) | Inside the in-memory window: any block, for keys tracked at that block. Older: archived subscriptions only ([archive](archive.md)). |
| `pending` | The latest state (skim keeps no mempool), except for `eth_getTransactionCount`. |
| `safe` | The highest execution block a 2/3 supermajority of the sync committee signed off on (light client). `-32002` until verified. |
| `finalized` | The execution block of the latest finalized beacon block skim verified. `-32002` until verified. |
| `earliest` | Not tracked: `-32002`. |

**Light-client status:** bootstrap and usable signed updates are required. `safe` and `finalized` return `-32002` until the corresponding execution hash is verified and available on the selected chain. The path has been verified on a minimal-preset devnet; earlier public Sepolia runs encountered missing light-client data. Check `skim_status.light_client` for the current node's result rather than assuming all peers serve it. See [trust and correctness](trust-and-correctness.md).

## Errors
| Code | When |
|---|---|
| `-32002` | Resource unavailable: no peer could serve the state, receipts or headers right now, or the block is outside the window and not archived. **skim never substitutes a guess.** `error.data` says why and whether retrying can help: `{"reason": …, "retry": true/false}` (below). |
| `-32004` | Explicit unsupported methods: `eth_sendTransaction` (skim holds no keys; sign in your client), `eth_newFilter`, `eth_getFilterChanges`, and `eth_subscribe` over HTTP. WebSocket subscriptions are supported; other unknown methods return `-32601`. |
| `-32005` | `eth_getLogs` range larger than `--max-logs-range`. |
| `-32004` | History below the discovered Glamsterdam fork floor. Missing state outside retained/archived coverage instead returns `-32002`. |
| `-32000` | Transaction rejected (`nonce too low`, `insufficient funds…`, invalid signature, wrong chain id), or EVM halt. |
| `3` | Execution reverted. `data` carries the revert bytes, so viem/ethers decode custom errors as usual. |
| `-32602` / `-32601` | Bad params / unknown method. |

### Why `-32002`: `error.data.reason`
Stable identifiers, so an app can decide what to do without parsing messages:

| `reason` | `retry` | Meaning |
|---|---|---|
| `starting` | yes | skim hasn't applied its first block yet |
| `no_state_peers` | yes | no SNAP peer connected yet (the first minutes after a cold start); retry in a few seconds |
| `state_incomplete` | yes | a call's state needed more fetch rounds than allowed (`on_demand.max_fetch_rounds`) |
| `not_served` | yes | peers didn't serve a header, body, receipts or BAL just now |
| `chain_changed` | yes | a reorg happened during the request; ask again |
| `reorged` | yes | the requested block left the chain; ask at a current block |
| `not_verified` | yes | `safe`/`finalized` (or `--require-verified`): no verified block yet |
| `no_header` | yes | the block's header isn't held yet |
| `outside_window` | no | the block is older than `--window` and not archived |
| `outside_retention` | no | older than every connected peer still serves (expired receipts, or a state root peers no longer keep) |
| `not_archived` | no | an archive read outside the key's covered range |
| `not_indexed` | no | a transaction not in the index (`--tx-index`) |
| `not_held` / `key_limit` | no | warm-only mode without the state; the subscribed-key limit refused it |
| `fork_unsupported` | no | `latest`/`pending` after the chain passed a fork skim doesn't implement ([Hegota](hegota.md)): skim's head is frozen before it. Explicit blocks and `safe`/`finalized` still answer |
| `unavailable` | yes | anything else |

`eth_accounts` and `eth_requestAccounts` return `[]`: skim holds no keys. Browser pages can call skim directly: responses carry `Access-Control-Allow-Origin` per `--cors` (default `*`).

## Not yet
`flatCallTracer`, JS tracers, `newPendingTransactions`, blob fee fields in `eth_feeHistory`, tracing transactions older than `--window`.

## Compatibility notes
- **Checked against references:** state overrides, `eth_simulateV1` (including validation), all tracers, block data, receipts, `eth_getProof`, `eth_feeHistory` and `eth_createAccessList` were checked against Nethermind and geth at the same block. Everything was identical (`sim_trace_check.py`, `rpc_gaps_check.py`).
- **Where the clients disagree, skim follows geth:**
  - `eth_feeHistory` rewards: Nethermind computes them differently, and `eth_createAccessList` returned no list on Nethermind.
  - `muxTracer`: Nethermind doesn't implement it. skim's mux output equals its standalone tracers.
- **Calls without `gas`:** skim uses the block gas limit; nodes differ here (Nethermind: 100M).
- **EIP-8037 gas breakdown in `callTracer`:** the field names aren't standardized across clients yet. skim says `executionGasUsed`, Nethermind `regularGasUsed`.
- **Fee recipient in `prestateTracer`:** it is included, as geth and Nethermind do, even though skim's calls charge no fees.
- **Failed transactions:** when execution fails before entering any frame (EIP-8037 state gas for a new account charged up front), the `callTracer` root frame is built from the transaction, with `error: "out of gas"`, as the other clients do.
- **Some error strings differ:** for example, skim says "out of gas: not enough gas for reentrancy sentry" where Nethermind says "out of gas".
- **Replay check:** compared with Nethermind on real Sepolia transactions, `callTracer` was identical for 20 of 20 transactions, and `prestateTracer` and the default tracer matched. A whole-block trace (156 transactions) differed only in the error string above (`replay_check.py`).
