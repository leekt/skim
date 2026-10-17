# Examples (viem)

Runnable apps that use skim as their Ethereum endpoint. The scripts are plain [viem](https://viem.sh): the only skim-specific part is the URL, plus the `skim.yaml` that tells skim what to keep ready.

```bash
cargo build --release                                   # once, from the repo root
cd examples && npm install                              # viem 2.57.1, pinned
```

| Example | Shows |
|---|---|
| [`uniswap-v3/`](uniswap-v3) | pool state, quotes for any amount, a quote every block, `Swap` logs, readiness |
| [`uniswap-v4/`](uniswap-v4) | `StateView` pool state, V4 quoter in both directions, `PoolManager` logs filtered by pool id |
| [`websocket/`](websocket) | live pushes over WebSocket: new blocks, `Swap` logs, and every block's subscription readiness and `on_block` quote |
| [`bigname/`](bigname) | ENS for the [bigname](https://github.com/ensdomains/bigname) indexer: skim as its verified-lookup and hydration RPC, checked name by name against a reference node |
| [`smallname/`](smallname) | bigname's `/v1` ENS API (names, records, subnames, names by address, search, history) from skim alone: live reads at a block hash plus a 20 MB SQLite index of post-Glamsterdam events, with a test page |
| [`send-transaction/`](send-transaction) | sign locally, `sendTransaction` → skim gossips it → `waitForTransactionReceipt` |

## Uniswap V3
```bash
../target/release/skim run --config uniswap-v3/skim.yaml          # terminal 1 (wait for "state ready")
npm run v3                                                         # terminal 2
```
```text
pool  tick 169006  liquidity 8790752292368793  sqrtPriceX96 370352887563927874543762520788062  (23.6 ms)
quote     1 WETH ->   44546.304171 USDC  ticks crossed 3  (3.8 ms)
quote  0.37 WETH ->   16731.823818 USDC  ticks crossed 3  (2.3 ms)
quote  12.5 WETH ->  454175.524311 USDC  ticks crossed 24  (3.2 ms)
block 11919449: 1 WETH -> 44546.304171 USDC  (5.4 ms)
block 11919450: 1 WETH -> 44546.304171 USDC  (4.2 ms)
block 11919451: 1 WETH -> 44546.304171 USDC  (3.0 ms)
subscription quote-1-weth: ready=true block=11919451 keys=39
subscription weth-usdc-0.3%: ready=true block=11919451 keys=5
```
- **Every number matches a reference node**: the same script pointed at another Sepolia RPC (`SKIM_RPC=<url>`) prints identical values.
- **The 12.5 WETH quote** crossed 24 ticks it had never seen and still took 3 ms. The V3 predictor in `skim.yaml` keeps the tick window around the price warm.

**What the config does:**
- **Subscribes** the 0.3% pool's `slot0` and `liquidity`, so they're applied from every block's access list.
- **Subscribes** a 1 WETH quote call with `on_block`. skim re-runs it every block and keeps its 39-key access set warm.
- **Enables** the V3 predictor for the three WETH/USDC pools.

## Uniswap V4
```bash
../target/release/skim run --config uniswap-v4/skim.yaml
npm run v4
```
```text
pool  tick -169060  lpFee 20  liquidity 31404138024165862  sqrtPriceX96 16903753027666117375455193  (21.7 ms)
quote 0.001 ETH -> 45.519289 USDC  (2.6 ms)
quote 0.0042 ETH -> 191.176859 USDC  (1.9 ms)
quote 5 USDC -> 0.000109838320618968 ETH  (170.1 ms)
3 Swap events for this pool in the last 2,000 blocks (3674.4 ms)
```
V4 keeps every pool inside one `PoolManager`, so there's no pool address to subscribe. The config **subscribes the calls**: `StateView.getSlot0(poolId)` and a quote. skim learns which `PoolManager` slots they read (this pool's state, bitmap and ticks) and keeps only those current. The `5 USDC → ETH` quote goes the other direction, so it needed one fetch (170 ms) the first time; repeat it and it's local.

## WebSocket pushes
skim serves WebSocket on the same address as HTTP:
```bash
npm run ws
```
```text
block 11921517 ready
  quote-1-weth: ready=true, 1 WETH -> 44291.088036 USDC
  weth-usdc-0.3%: ready=true
```
viem's `watchBlockNumber`, `watchBlocks`, `watchEvent` and `watchContractEvent` use `eth_subscribe` over WebSocket with no changes. `skim_subscriptions` is skim's own subscription: after every block, it pushes each subscription's readiness and its latest `on_block` results.

## Sending a transaction
```bash
PRIVATE_KEY=0x… npm run send -- 0xRecipient 0.001
```
1. viem signs locally. Nonce (`pending`), gas and fees come from skim.
2. `eth_sendRawTransaction` is checked against verified state, then gossiped to skim's EL peers.
3. `waitForTransactionReceipt` polls skim. The receipt comes from the including block's peers, checked against `receiptsRoot`.

```text
from 0x6011…0491  balance 0.05 ETH
sent 0xeb9b34c0…1e50
included in block 11924541 (index 147), status success, gas used 12000, after 4.3 s
```
The receipt and transaction skim served were identical to a reference node's.

## Adapting these to your app
1. **Subscribe what you read.** List the slots if you know them; otherwise subscribe the calls your app makes. See the [configuration reference](../docs/guide/configuration.md).
2. **Point your client at skim.** `http('http://127.0.0.1:8545')` in viem, `--rpc-url` in Foundry.
3. **Gate on readiness** if you need it: `skim_subscriptionStatus` → `ready`.
