# Examples (viem)

Runnable apps that use skim as their Ethereum endpoint. The scripts are plain [viem](https://viem.sh): the only skim-specific part is the URL, plus the `skim.yaml` that tells skim what to keep ready.

These configs and scripts use Sepolia addresses and viem's Sepolia chain definition. `skim run` also follows custom Glamsterdam networks, but using these examples there requires replacing contracts and client chain settings; changing `network:` in YAML does not select a chain. See [network configuration](../docs/guide/configuration.md#networks) and the separate [devnet check](../devnet/README.md).

```bash
cargo build --release                                   # once, from the repo root
cd examples && npm install                              # viem 2.57.1, exact version
```

Run the commands below from `examples/`. Wait for the relevant `skim_subscriptionStatus` entries to be ready, not just the first `state ready` line: the latter means a block was applied, and subscribed state may still be fetching. Recorded output and timings below are observations from Sepolia runs.

| Example | Shows |
|---|---|
| [`uniswap-v3/`](uniswap-v3) | pool state, quotes for any amount, a quote every block, `Swap` logs, readiness |
| [`uniswap-v4/`](uniswap-v4) | `StateView` pool state, V4 quoter in both directions, `PoolManager` logs filtered by pool id |
| [`websocket/`](websocket) | live pushes over WebSocket: new blocks, `Swap` logs, and every block's subscription readiness and `on_block` quote |
| [`bigname/`](bigname) | ENS for the [bigname](https://github.com/ensdomains/bigname) indexer: skim as its verified-lookup and hydration RPC, checked name by name against a reference node |
| [`smallname/`](smallname) | bigname, but small: an ENS API with skim serving current state and a small SQLite index for discovery and history; with a test page |
| [`send-transaction/`](send-transaction) | sign locally, `sendTransaction` → skim gossips it → `waitForTransactionReceipt` |
| [`watch.mjs`](watch.mjs) | a read-only wallet with no config: ETH, nonce and ERC-20 balances, then each block's changes and `Transfer` logs over WebSocket |
| [`wallet/`](wallet) | a CLI wallet: Web3 Secret Storage v3 keystores, then `send` built from skim's JSON-RPC, confirmed, signed locally, and followed to its receipt. A demo, not audited key management |
| [`devnet-edgecases.mjs`](devnet-edgecases.mjs) | state edge cases (creation, selfdestruct, slot clearing, EIP-7702, reverts) against an executing node on a local devnet; see the [devnet README](../devnet/README.md#state-edge-cases) |

smallname's own tests (no network) run with `npm run -s smallname-test`; the wallet's keystore tests with `npm run -s wallet-test`.

Wallet features stay out of skim itself: skim holds no keys and has no wallet commands. `watch.mjs` and `wallet/` show that a wallet needs nothing beyond the standard JSON-RPC and WebSocket skim serves.

## Uniswap V3
```bash
../target/release/skim run --config uniswap-v3/skim.yaml          # terminal 1 (wait for the subscriptions to be ready)
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
skim serves WebSocket on the same address as HTTP. With skim running the Uniswap V3 config (the subscription names below come from it):
```bash
npm run ws                                                         # SKIM_WS overrides ws://127.0.0.1:8545
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
3. `waitForTransactionReceipt` polls skim. skim tracks transactions sent through it (pending, then included), so they are found whatever `--tx-index` is set to. The receipt comes from the including block's peers, checked against `receiptsRoot`.

```text
from 0x6011…0491  balance 0.05 ETH
sent 0xeb9b34c0…1e50
included in block 11924541 (index 147), status success, gas used 12000, after 4.3 s
```
The receipt and transaction skim served were identical to a reference node's.

## Watching addresses (read-only wallet)
No config: skim serves every state read on demand, fetched from peers and checked against the block's state root.
```bash
../target/release/skim run                                         # terminal 1, no --config
npm run watch -- 0x7b2f45a62a2dbe86287716b37c7b2ebedfd57d66 0x3eddfb11902b5de1f1637e4d195573f566406ebc
```
```text
balances at block 11950489
  0x7B2F45a62a2dbE86287716b37c7b2eBEDFd57D66  10.676144204651641602 ETH  nonce 43082  0 WETH  0 USDC
  0x3eddfb11902B5DE1f1637e4d195573f566406ebc  6.261742335638746489 ETH  nonce 135685  0 WETH  0 USDC
  (107 ms; first reads of an account can take a peer round trip)
following new blocks over ws://127.0.0.1:8545
block 11950490  (read at block 11950490, 69 ms)
  in  0.001 APD_USDC  0x048E…7Dc9 -> 0x7B2F…7D66  tx 0x7cdc…0cda
  in  0.002 APD_USDC  0x048E…7Dc9 -> 0x3edd…6ebc  tx 0xcbeb…f8b5
  …
```
- **At start:** ETH balance, nonce and ERC-20 balances, all read at one block. Default tokens are Sepolia WETH and USDC, the tokens the Uniswap examples use. `--token 0x…` (repeatable) replaces them; `--no-tokens` drops them.
- **Every block** (`eth_subscribe("newHeads")`): `eth_getBalance` and `eth_getTransactionCount` at that block for each address, plus two `eth_getLogs` for ERC-20 `Transfer` logs from or to the addresses, in any token. A watched token's balance is re-read only when one of its transfers touches an address. A nonce change means an outgoing transaction; the block's transactions from that address are listed. Every line says which block its values were read at.
- **Reconnects** when the WebSocket closes, backing off from 1 s to 10 s. Blocks missed meanwhile are caught up: logs over the gap (up to 256 blocks), balances at the new head.
- `WATCH_QUIET=1` hides blocks with no change. `SKIM_RPC` / `SKIM_WS` override the endpoints.

## CLI wallet
> **A demo, not audited key management.** Keys are decrypted into this Node process, which can't wipe them from memory. There is no hardware wallet support, and the code has not been reviewed. Use test keys and testnet funds.

```bash
npm run wallet -- new                                   # create a key; password prompted with echo off
npm run wallet -- import < key.txt                      # or WALLET_PRIVATE_KEY=0x… npm run wallet -- import
npm run wallet -- list                                  # keystores, numbered
npm run wallet -- address                               # the selected account
npm run wallet -- balance                               # ETH; `balance 0xToken` for an ERC-20
npm run wallet -- send 0xRecipient 0.001                # ETH
npm run wallet -- send 0xRecipient 5 --token 0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238   # USDC
```
```text
from      0x008AeEda4D805471dF9b2A5B0f38A0C3bCBA786b
to        0x00000000000000000000000000000000000000bb
amount    0.1 ETH
chain id  11155111   nonce 3   gas 21000
fees      base ~1.01 gwei, tip 1 gwei, max 3.02 gwei
max cost  0.10006342 ETH  (balance 1 ETH)
send? [y/N] y
password:
sent 0x7e15a0bb…7fab
included in block … (index …), status success, gas used 21000, fee … ETH, after … s
```
(This transcript is from the offline test against a mock endpoint, not a Sepolia send.)

- **Keystores** are [Web3 Secret Storage v3](https://ethereum.org/en/developers/docs/data-structures-and-encoding/web3-secret-storage/) JSON files: scrypt (n=262144, r=8, p=1, geth's standard), aes-128-ctr, and a keccak256 MAC that rejects a wrong password before decrypting. They are stored in `~/.skim-wallet/` (`WALLET_DIR` overrides), the directory mode 0700 and files 0600, with geth's `UTC--…--<address>` names, so geth, Foundry (`--keystore`) and MetaMask (JSON file import) can use them. pbkdf2 keystores from elsewhere decrypt too.
- **Passwords** come from `WALLET_PASSWORD` or a prompt on the terminal (`/dev/tty`) with echo off.
- **Private keys never come from arguments**, which end up in shell history and `ps`. `import` reads `WALLET_PRIVATE_KEY`, piped stdin, or a hidden prompt.
- **Account:** the first keystore, or `--from <address|index>` (`WALLET_FROM`).
- **`send`** asks skim for everything: `eth_chainId`, `eth_getTransactionCount(…, "pending")`, fees from `eth_feeHistory` (next block's base fee, median tip) and `eth_maxPriorityFeePerGas`, `maxFeePerGas = 2 × base fee + tip`, and gas from `eth_estimateGas`. It shows a summary and asks for confirmation (`--yes` skips it), then decrypts the key, signs an EIP-1559 transaction with viem, sends it with `eth_sendRawTransaction`, and polls for the receipt.
- **Zero-config works:** `balance` and `send` read only the sender's account and, for tokens, `balanceOf`/`decimals`/`symbol`, which skim fetches on demand.
- **Tests** (no network): `npm run -s wallet-test`. They cover an encrypt → decrypt round trip, the spec's scrypt and pbkdf2 test vectors (password `testpassword`, key `7a28b5ba…fe9d`), and wrong-password and tampered-ciphertext rejection by the MAC. The spec's scrypt vector uses n=262144 with r=1, which OpenSSL refuses (RFC 7914 requires N < 2^(16r)). Those parameters are derived with `@noble/hashes` (installed as viem's dependency); everything else uses Node's built-in scrypt.

## Adapting these to your app
1. **Subscribe what you read.** List the slots if you know them; otherwise subscribe the calls your app makes. See the [configuration reference](../docs/guide/configuration.md).
2. **Point your client at skim.** `http('http://127.0.0.1:8545')` in viem, `--rpc-url` in Foundry.
3. **Gate on readiness** if you need it: `skim_subscriptionStatus` → `ready`.
