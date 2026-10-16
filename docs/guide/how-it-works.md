# How skim works

skim answers Ethereum JSON-RPC for **the state your app uses**, without executing or storing the chain. Four ideas make that work.

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
The first time a request reads an account or slot skim doesn't track, it asks EL peers over **SNAP** (`snap/1` from Geth, `snap/2` from Nethermind):

```text
read slot x of C at head N
  ─► SNAP request at the state root of N−1 (peers already have it)
  ─► Merkle proof: account against stateRoot, slot against the account's storageRoot
  ─► replay BAL(N) on top  ─► value at N, now tracked: every later BAL keeps it current
```

**Rules that keep it correct:**
- A slot is zero **only when proven** (absent account, empty trie, or an exclusion proof). An unproven answer is retried with another peer, never assumed.
- **Unknown is never zero.** If no peer can serve a value, the request fails with `-32002` instead of returning something wrong.

The fetch scheduler adapts per-peer concurrency (AIMD on latency), hedges slow requests, deduplicates, and keeps a slot free for interactive calls over background work.

## 3. Subscribed vs on-demand state
| | Subscribed (in `skim.yaml`) | On demand (everything else) |
|---|---|---|
| Kept current | from every block's BAL, forever | for `cache_ttl_blocks` after last use |
| Hot path | **no network** | one verified fetch wave the first time |
| Readiness | reported per subscription (`skim_subscriptionStatus`) | best effort |

A subscription can name accounts and slots, or **calls**. For a call, skim runs it every block, records every key it reads (its *access set*), and keeps those keys subscribed. You don't need to know storage layouts: subscribe the `quote()` your app makes and skim keeps exactly the slots it reads.

## 4. Predicting what a call will read
A new call input (a different amount, another pool) may read keys skim doesn't track yet. Fetching them one EVM round at a time would take several network round trips. The **State Access Predictor** guesses them up front and fetches them in one wave:

- **Learned sets:** the access set of the same input, otherwise of *similar* inputs (same contract, selector, addresses and small arguments; amounts in the same order of magnitude), otherwise the keys common to most runs of that function.
- **Protocol predictors:** for Uniswap V3 (`predictors.uniswap_v3`), the tick range a swap of the requested size can reach, from the pool's current price and liquidity. A window around the current tick is also kept warm in the background.

After running, any key the prediction missed is fetched in another round. A missed tick-bitmap word pulls in the whole word range at once.

## 5. Executing
Calls run in [revm](https://github.com/bluealloy/revm) with Amsterdam rules, against a snapshot of skim's per-key, multi-version store at the requested block. Reads at earlier blocks inside the in-memory window (`--window`, default 256 blocks) work for keys skim was already tracking at that block. Peers serve SNAP only for recent state roots, so other old reads may answer `-32002`. Beyond the window, archived subscriptions answer at any block they cover ([archive](archive.md)).

## 6. Transactions, receipts and logs
- **`eth_sendRawTransaction`:**
  - checks the transaction against verified head state (chain id, nonce, balance)
  - sends it straight to every connected EL peer
  - re-broadcasts it until a gossiped payload includes it
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
| Headers, tx hashes, BAL addresses | memory, last 2,048–8,192 blocks | a few MB |
| Recent blocks and envelopes for serving | memory: BALs of the last 64 blocks, bodies of 128, envelopes of 64 slots | ~10–25 MB |
| Restart snapshot | `redb` file in `--data-dir` (default `~/.skim/sepolia`): subscribed values, node key, peers | ~1 MB for 2,400 keys |
| Archived history (optional) | `redb` file, only for `retention: archive` subscriptions | versions only on change |

Measured with the Uniswap V3 config: ~200 MB RSS once the default 256-block window has filled (about 42 MB of it is the recent-BAL index), ~0.25 CPU core. `--window 32` keeps it near 100 MB. On disk: the restart snapshot (about 1 MB), plus the archive if you enable one. `--ephemeral` keeps nothing on disk.
