# How skim works

skim answers Ethereum JSON-RPC for **the state your app uses**, without re-executing every block or storing the full chain state. It combines BAL following, verified fetches, subscriptions and local simulation.

`skim run` defaults to Sepolia and accepts other Glamsterdam networks through `--network <dir>` ([configuration](configuration.md#networks)). Chain id, consensus preset, slot duration, genesis and peer discovery come from that network's configuration. Consensus peers are dialed over QUIC when they announce it, with TCP as the fallback.

## 1. Every block tells skim what changed
Since Glamsterdam, each block carries a **block-level access list** ([EIP-7928](https://eips.ethereum.org/EIPS/eip-7928)): every account and storage slot the block touched, with the **value after** each write. With ePBS, that list travels inside the execution payload gossiped on the consensus layer.

```text
CL gossip: execution_payload ──► rebuild the EL header from the payload
                                  └─ keccak(header) == block_hash ?  (binds the BAL to the block)
                              ──► apply BAL writes to the keys skim tracks
                              ──► "state ready" for block N
```
- **No execution:** if skim knows slot `x` of contract `C` at block N−1 and block N's BAL says nothing about it, the value is unchanged. If the BAL writes it, the BAL carries the new value.
- **Speed:** applying a block takes ~0.2 ms.
- **Gaps:** gaps from a restart or missed gossip are filled from execution-layer peers (eth/71 `GetBlockAccessLists`).

## 2. State skim hasn't seen is fetched, and proven
A key skim doesn't track is resolved in this order:
1. **Local:** subscribed or on-demand state skim already holds.
2. **A recent BAL:** if a block in the window (`--window`, default 256 blocks) wrote the key, the latest such write is its value, as long as every block since that write is indexed. Nothing is fetched. State that changed recently, like the balance of an account that just sent a transaction, is answered immediately.
3. **SNAP:** otherwise skim asks EL peers (`snap/1` from Geth, `snap/2` from Nethermind):

```text
read slot x of C at head N
  ─► SNAP request at the state root of N−1 (peers already have it)
  ─► Merkle proof: account against stateRoot, slot against the account's storageRoot
  ─► replay BAL(N) on top  ─► value at N, now tracked: every later BAL keeps it current
```

**Rules that keep it correct:**
- A slot is zero **only when proven** (absent account, empty trie, or an exclusion proof). An unproven answer is retried with another peer, never assumed.
- **Unknown is never zero.** If no peer can serve a value, the request fails with `-32002` instead of returning something wrong.

The fetch scheduler adapts per-peer concurrency (AIMD on latency), hedges slow requests, deduplicates, and keeps a slot free for interactive calls over background work. A request is hedged (sent once more, to another peer) when it has run well past the asked peer's own typical latency (its mean plus four deviations, between 150 ms and 1.2 s), and hedges stay under 10% of requests.

## 3. Subscribed vs on-demand state
| | Subscribed (in `skim.yaml`) | On demand (everything else) |
|---|---|---|
| Kept current | from every block's BAL, forever | from every block's BAL, for `cache_ttl_blocks` after last use (any read counts: `eth_call`, `eth_getStorageAt`, `eth_getBalance`, simulate, trace) |
| Hot path | **no network** | one verified fetch wave the first time |
| Readiness | reported per subscription (`skim_subscriptionStatus`) | best effort |

Readiness also requires the bytecode of subscribed accounts, not just their code hash. Otherwise the first call would fetch it.

A `storage: all` subscription's `max_slots` applies for the contract's whole life. Slots first written after the download count too. If the contract outgrows the limit, skim stops tracking it and the subscription reports the error.

A subscription can name accounts and slots, or **calls**. For a call, skim runs it every block, records every key it reads (its *access set*), and keeps those keys subscribed. You don't need to know storage layouts: subscribe the `quote()` your app makes and skim keeps exactly the slots it reads.

## 4. Predicting what a call will read
A new call input (a different amount, another pool) may read keys skim doesn't track yet. Fetching them one EVM round at a time would take several network round trips. The **State Access Predictor** guesses them up front and fetches them in one wave:

- **Learned sets:** the access set of the same input, otherwise of *similar* inputs (same contract, selector, addresses and small arguments; amounts in the same order of magnitude), otherwise the keys common to most runs of that function.
- **Protocol predictors:** for Uniswap V3 (`predictors.uniswap_v3`), the tick range a swap of the requested size can reach, from the pool's current price and liquidity. A window around the current tick is also kept warm in the background. That window is a hot set, not a subscription: its keys stay cached while they are near the price, and expire `window_ttl_blocks` after the price moves away.

After running, any key the prediction missed is fetched in another round. A missed tick-bitmap word pulls in the whole word range at once.

## 5. Executing
Calls run in [revm](https://github.com/bluealloy/revm) with Amsterdam rules, against a snapshot of skim's per-key, multi-version store at the requested block. Reads at earlier blocks inside the in-memory window (`--window`, default 256 blocks) work for keys skim was already tracking at that block. Peers serve SNAP only for recent state roots, so other old reads may answer `-32002`. Beyond the window, archived subscriptions answer at any block they cover ([archive](archive.md)).

## 6. Transactions, receipts and logs
- **`eth_sendRawTransaction`:**
  - checks the transaction against verified head state (chain id, nonce, balance)
  - sends it straight to every connected EL peer
  - re-broadcasts it until a gossiped payload includes it
- **Lookups by hash** (`eth_getTransactionByHash`, `eth_getTransactionReceipt`, `debug_traceTransaction`): as payloads arrive, skim indexes transaction hashes (8-byte prefixes, confirmed against the verified body on lookup) for the last `--tx-index-blocks` blocks (2,048). By default (`--tx-index pinned`) it indexes only transactions whose BAL entries change a pinned address, so it keeps nothing about the rest of the chain; `--tx-index all` indexes every transaction. Transactions sent through skim are tracked separately and always found.
- **Receipts:** fetched with `GetReceipts` for that one block, and accepted only if they rebuild the header's `receiptsRoot`.
- **`eth_getLogs`:**
  - scans headers in the range
  - a block is skipped when its `logsBloom` can't match
  - for blocks skim followed live, a block is also skipped when no filtered address is in its BAL, because an emitting contract always is
  - fetches and verifies receipts only for the remaining blocks

## 7. What skim serves back
skim answers other peers' requests from recent data it has already verified against block headers. It doesn't pass on anything it hasn't verified.

| Protocol | Request | Served from |
|---|---|---|
| eth/69–71 | `GetBlockHeaders` | canonical headers skim followed (rising, falling, skip) |
| eth/69–71 | `GetBlockBodies` | gossiped payloads: transactions and withdrawals, both checked against the header (last 128 blocks) |
| eth/69–71 | `GetReceipts` (all versions) | receipts skim fetched and verified for its own users (`receiptsRoot`) |
| eth/71 | `GetBlockAccessLists` | raw BALs of the last 64 blocks, re-checked against `block_access_list_hash` before sending |
| snap/2 | `GetByteCodes`, `GetBlockAccessLists` | bytecode skim holds (hash checked); BALs as above |
| snap/2 | `GetAccountRange`, `GetStorageRanges` | empty ("not available"): skim holds no state tries |
| CL | `ExecutionPayloadEnvelopesByRange` / `ByRoot` | verified envelopes of the last 64 slots, only those on the chain skim applied |

- **Unknown items:** anything skim doesn't hold gets the protocol's "don't have it" answer right away (a shorter or empty response), so the peer doesn't wait for a timeout.
- **Advertised range:** eth/69+ peers are told the range skim holds without gaps (`BlockRangeUpdate`), ending at the block skim last applied.
- **Counters:** `skim_status.served` shows requests and items per message type.
- **Opting out:** `--no-serve` turns serving off.

## What skim keeps
| Data | Where | Size |
|---|---|---|
| Tracked state | memory, multi-version for `--window` blocks | ~10–100 MB for typical app state |
| Contract code | memory, only for tracked accounts | per contract |
| Headers, BAL addresses, transaction index | memory: headers of the last 8,192 blocks, BAL addresses of 2,048, the transaction index for `--tx-index-blocks` (2,048) | fills over hours; on a build that indexed every transaction, the index group was ~36 MB of payload at ~7 h ([hardware](hardware.md#memory)) |
| Recent blocks and envelopes for serving | memory: BALs of the last 64 blocks, bodies of 128, envelopes of 64 slots | ~10–25 MB |
| Restart snapshot | `restart.redb` in `--data-dir` (default `~/.skim/<network>`): subscribed values, node key, peers, verified finalized root | ~1 MB for 2,400 keys |
| Chain-data history (optional) | `history.redb` with `--history local`: post-fork headers and fetched matching logs | grows with headers and requested log coverage |
| Archived history (optional) | `redb` file, only for `retention: archive` subscriptions | versions only on change |

Measured on Sepolia (Linux aarch64, `--history local` plus an indexer): 179 MB RSS at 26 minutes; on a build that indexed every transaction, 346 MB at 9 h, flat after ~7 h. Short macOS soaks with `--window 32` had 99–105 MB medians. These are workload-specific observations, not memory ceilings. `skim_status.memory.parts` estimates retained payloads but omits network-buffer sizes and allocator overhead; see [hardware requirements](hardware.md). `--ephemeral` disables restart persistence and rejects `--history local`; an explicit `--archive` still writes its file.

## Consensus verification

The light client bootstraps a sync committee from a trusted checkpoint, verifies BLS-signed updates and their Merkle branches, and follows committee rotations using the configured consensus preset. `safe` names a supermajority-signed execution block; `finalized` names a verified finalized block, each checked against the chain skim follows. `--require-verified` uses the signed block for `latest` too. Default `latest` remains the newest state-ready gossip block. Verification depends on peers serving the required light-client data; inspect `skim_status.light_client` rather than assuming it has bootstrapped. This path has been verified on the minimal-preset [devnet](../../devnet/README.md).
