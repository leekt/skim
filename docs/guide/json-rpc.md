# JSON-RPC reference

skim serves standard Ethereum JSON-RPC over HTTP and WebSocket on the same address (`--rpc`, default `127.0.0.1:8545`; `ws://` for WebSocket), with single and batch requests. Unmodified viem, ethers, web3.py and Foundry (`cast`, `forge script --rpc-url`) work against it.

## Methods
| Method | Notes |
|---|---|
| `eth_call` | Local revm. Unknown state is fetched (verified) first. Honours `gasPrice` / `maxFeePerGas` / `maxPriorityFeePerGas` (0 if omitted, as nodes do), `accessList`, `authorizationList` (EIP-7702) and **state overrides** (`balance`, `nonce`, `code`, `state`, `stateDiff`). Blob fields are rejected (`-32602`), never ignored. |
| `eth_simulateV1` | Sequences of calls across simulated blocks (each call sees earlier calls' writes), with per-block `blockOverrides` (number, time, gas limit, fee recipient, base fee, prevrandao) and `stateOverrides`. Per call: `status`, `returnData`, `gasUsed`, `logs`, `error`. `validation: true` checks nonces, balances and EIP-1559 base fees (simulated blocks get proper base fees) with the spec's error codes (e.g. `-38012`). `traceTransfers`: on Glamsterdam, ETH transfers already emit native EIP-7708 logs, so these are what you get. |
| `debug_traceCall` | geth tracers: default (struct logs), `callTracer`, `prestateTracer`, `4byteTracer`, `muxTracer`, `noopTracer`, with `stateOverrides` and `blockOverrides`. Not yet: `flatCallTracer`, JS tracers. |
| `eth_createAccessList` | geth's algorithm (iterate until the list is stable; gas is measured with the final list). |
| `eth_getProof` | EIP-1186 proofs assembled from SNAP answers and verified against the block's state root before they're returned. Only for recent blocks, because peers serve only recent state roots. |
| `eth_feeHistory` | Base fees (including the next block's) and gas-used ratios from headers; reward percentiles with geth's algorithm, from verified bodies and receipts. Blob fee fields are omitted (the blob-parameter schedule isn't tracked yet). |
| `eth_getBlockReceipts`, `eth_getTransactionByBlock{Number,Hash}AndIndex`, `eth_getBlockTransactionCountBy{Number,Hash}`, `eth_getUncle*` | From verified bodies and receipts; uncles are always empty. |
| `eth_estimateGas` | Binary search on local execution, once the first run made the state local. |
| `eth_getBalance`, `eth_getCode`, `eth_getStorageAt` | Tracked: local. Untracked: one verified fetch. |
| `eth_getTransactionCount` | `pending` adds transactions sent through this skim and not yet included (gap-free nonces only). |
| `eth_sendRawTransaction` | Checks chain id, signature, `nonce too low`, and `insufficient funds` against verified head state. Then gossips to all connected EL peers, re-broadcasting every 12 s until included (gives up after 30 min). Blob transactions are rejected. |
| `eth_getTransactionByHash` | Transactions sent through skim (pending or included), and any transaction in the last 2,048 indexed blocks. |
| `eth_getTransactionReceipt` | `null` until included. Then built from the block's receipts, fetched from peers and checked against `receiptsRoot`. |
| `eth_getLogs` | Address / topics / `fromBlock`–`toBlock` / `blockHash`. Range ≤ `--max-logs-range` (2,048). Only blocks whose bloom (and, for followed blocks, BAL) can match have their receipts fetched; receipts are verified. |
| `eth_getBlockByNumber`, `eth_getBlockByHash` | Header fields plus transaction hashes, or full transaction objects with `true`. Bodies are verified against `transactionsRoot`. |
| `eth_blockNumber` | The latest **state-ready** block. |
| `eth_chainId`, `net_version`, `net_listening`, `eth_syncing`, `web3_clientVersion` | |
| `eth_gasPrice`, `eth_maxPriorityFeePerGas` | Head base fee + 1 gwei tip, and the 1 gwei tip. |
| `skim_subscriptionStatus` | Per subscription: `ready`, `block`, `required_keys`, `missing_keys`, `truncated`, `calls_ok`, `error`, `results` (for `on_block` calls). |
| `skim_status` | Head, real store sizes (subscribed / on-demand keys, bytecode, refused admissions), peer counts, light-client status. |

## WebSocket subscriptions
Every method works over WebSocket too. In addition, `eth_subscribe` / `eth_unsubscribe` support:

| Subscription | Pushes |
|---|---|
| `newHeads` | the header of each block whose state is ready |
| `logs` (with an `address` / `topics` filter) | each matching log of each new block, one notification per log |
| `skim_subscriptions` | after each block: every subscription's `ready`, `missing_keys`, `calls_ok`, `error` and latest `on_block` `results` |

viem's `watch*` helpers use these automatically with `webSocket(...)`.

## Block parameters
| Tag | Meaning |
|---|---|
| `latest` (or omitted) | The latest state-ready block. Pinned for a whole batch request. |
| a number, or `{blockNumber}` / `{blockHash}` (EIP-1898) | Inside the in-memory window: any block, for keys tracked at that block. Older: archived subscriptions only ([archive](archive.md)). |
| `pending` | The latest state (skim keeps no mempool), except for `eth_getTransactionCount`. |
| `safe` | The highest execution block a 2/3 supermajority of the sync committee signed off on (light client). `-32002` until verified. |
| `finalized` | The execution block of the latest finalized beacon block skim verified. `-32002` until verified. |
| `earliest` | Not tracked: `-32002`. |

**Light-client status:** consensus clients on Sepolia don't serve Gloas light-client data yet, so `safe` and `finalized` currently answer `-32002`. `skim_status.light_client` shows the state.

## Errors
| Code | When |
|---|---|
| `-32002` | Resource unavailable: no peer could serve the state, receipts or headers right now, or the block is outside the window and not archived. **skim never substitutes a guess**, so retrying later is safe. |
| `-32004` | Not supported: `eth_sendTransaction` (skim holds no keys; sign in your client), filters and subscriptions. |
| `-32005` | `eth_getLogs` range larger than `--max-logs-range`. |
| `-32000` | Transaction rejected (`nonce too low`, `insufficient funds…`, invalid signature, wrong chain id), or EVM halt. |
| `3` | Execution reverted. `data` carries the revert bytes, so viem/ethers decode custom errors as usual. |
| `-32602` / `-32601` | Bad params / unknown method. |

## Not yet
`debug_traceTransaction`, `flatCallTracer`, JS tracers, `newPendingTransactions`, blob fee fields in `eth_feeHistory`.

## Compatibility notes
- **Checked against references:** state overrides, `eth_simulateV1` (including validation), all tracers, block data, receipts, `eth_getProof`, `eth_feeHistory` and `eth_createAccessList` were checked against Nethermind and geth (Infura) at the same block. Everything was identical (`sim_trace_check.py`, `rpc_gaps_check.py`).
- **Where the clients disagree, skim follows geth:**
  - `eth_feeHistory` rewards: Nethermind computes them differently, and `eth_createAccessList` returned no list on Nethermind.
  - `muxTracer`: Nethermind doesn't implement it. skim's mux output equals its standalone tracers.
- **Calls without `gas`:** skim uses the block gas limit; nodes differ here (Nethermind: 100M).
- **EIP-8037 gas breakdown in `callTracer`:** the field names aren't standardized across clients yet. skim says `executionGasUsed`, Nethermind `regularGasUsed`.
- **Fee recipient in `prestateTracer`:** it is included, as geth and Nethermind do, even though skim's calls charge no fees.
