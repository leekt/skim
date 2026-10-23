# bigname on skim

[bigname](https://github.com/ensdomains/bigname) indexes ENS (ENSv1, ENSv2) into PostgreSQL and serves it over an HTTP API. It talks to Ethereum over JSON-RPC in two ways:

| bigname needs | RPC | skim |
|---|---|---|
| **Historical intake** from block 0: every ENS event since deployment | `eth_getLogs` over millions of blocks, `eth_getBlockByNumber` | **No full-history intake.** `--history local` can serve post-Glamsterdam history while peers or stored coverage supply it; pre-fork history still needs another source |
| **Live follow** at the head | `eth_getLogs` per block range, `eth_getBlockByHash`, receipts | Yes, for the blocks skim holds |
| **Verified lookups** (API `?verify`): Universal Resolver `eth_call` at a fixed block hash | `eth_call` with `{ blockHash }` | Yes. This is what `skim.yaml` keeps warm |
| **Hydration** (phase runner): resolver reads for names the indexer saw change | `eth_call` | Yes |

So skim takes the read load (verified lookups and hydration, the calls that scale with traffic) and the reference provider handles the one-time backfill.

**Limits:**
- This example's manifests, addresses and viem chain are Sepolia-specific. The node also accepts compatible custom Glamsterdam networks via `--network <dir>`, with app-specific contracts/configuration supplied separately.
- No Base or other L2s: those bigname sources need their own provider.
- A name skim hasn't seen takes a few fetch rounds the first time (0.3–1.6 s below), then stays local for `cache_ttl_blocks`.

## Run

From `examples/`, after installing its dependencies:

```bash
../target/release/skim run --config bigname/skim.yaml          # terminal 1 (wait for skim_subscriptionStatus to be ready)
SKIM_RPC=http://localhost:8545 REF_RPC=<another sepolia rpc> npm run ens -- ens.eth nick.eth vitalik.eth
```

Then point bigname at skim (in its `.env`):
```bash
BIGNAME_API_CHAIN_RPC_URLS=ethereum-sepolia=http://<skim-host>:8545
BIGNAME_PHASE_RUNNER_HYDRATION_RPC_URLS=ethereum-sepolia=http://<skim-host>:8545
# historical intake keeps its archive/log provider:
# SEPOLIA_INTAKE_RPC_URL=<archive or log provider>
```
skim listens on 127.0.0.1 by default. From bigname's containers, run skim with `--rpc 0.0.0.0:8545` on a private network, or with Docker use `host.docker.internal`.

## What `skim.yaml` keeps ready
- **Every ENS contract** bigname's Sepolia manifests declare: the account and code of the ENSv1 registry, registrar, NameWrapper, public resolvers and reverse registrars; ENSv2's root registry, `.eth` registry and registrar, and its resolvers; the Universal Resolver proxies and implementation.
- **Two Universal Resolver lookups**, re-run every block (`on_block`). They walk the shared path (proxy, implementation, ENSv2 root, `.eth` registry, the ENSv1 fallback, resolver), 51 keys, so any other name needs only its own few slots.
- **`on_demand.max_fetch_rounds: 24`**: a cold name's resolution goes several contracts deep, and each round fetches the next layer. With the default of 8 and no ENS subscriptions, a cold `ens.eth` lookup was refused.
- **`cache_ttl_blocks: 32`**: names bigname just hydrated stay local for about 6 minutes.

## Check: skim vs a reference node
`ens.mjs` reads each name through both nodes at the same block hash:
- `getEnsAddress` (viem, through the Universal Resolver)
- the raw `UniversalResolver.resolve` call, so reverts are compared by their data
- `getEnsName` for every address the names resolved to

Measured on Sepolia, 2.5 minutes after a cold start:
```text
block 11933182 0x6825f4576a0902615854317278715b5f310f00b073234a641fd434434edfc054
MATCH getEnsAddress ens.eth                    null  (skim 5.1 ms)
MATCH UR.resolve ens.eth @hash                 0x00000000…0000000000000000000000000000000000000000 (128 bytes)  (skim 7.8 ms)
MATCH getEnsAddress nick.eth                   0xb8c2C29ee19D8307cb7255e1Cd9CbDE883A267d5  (skim 7.0 ms)
MATCH UR.resolve nick.eth @hash                0x00000000…b8c2c29ee19d8307cb7255e1cd9cbde883a267d5 (128 bytes)  (skim 4.9 ms)
MATCH getEnsAddress vitalik.eth                0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045  (skim 4.7 ms)
MATCH UR.resolve vitalik.eth @hash             0x00000000…d8da6bf26964af9d7eed9e03e53415d37aa96045 (128 bytes)  (skim 2.2 ms)
MATCH getEnsAddress test.eth                   null  (skim 2.7 ms)
MATCH UR.resolve test.eth @hash                0x00000000…0000000000000000000000000000000000000000 (128 bytes)  (skim 0.9 ms)
MATCH getEnsAddress wrappedtest.eth            null  (skim 10.3 ms)
MATCH UR.resolve wrappedtest.eth @hash         revert 0x77209fe8…(100 bytes)  (skim 1.3 ms)
MATCH getEnsAddress doesnotexist12345.eth      null  (skim 3.2 ms)
MATCH UR.resolve doesnotexist12345.eth @hash   revert 0x77209fe8…(100 bytes)  (skim 6.5 ms)
MATCH getEnsAddress taek.eth                   null  (skim 285.7 ms)
MATCH UR.resolve taek.eth @hash                revert 0x77209fe8…(100 bytes)  (skim 0.9 ms)
MATCH getEnsAddress gregskril.eth              0x179A862703a4adfb29896552DF9e307980D19285  (skim 1579.6 ms)
MATCH UR.resolve gregskril.eth @hash           0x00000000…179a862703a4adfb29896552df9e307980d19285 (128 bytes)  (skim 1.4 ms)
MATCH getEnsName 0xb8c2C29e…                   null  (skim 4.4 ms)
MATCH getEnsName 0xd8dA6BF2…                   null  (skim 1.4 ms)
MATCH getEnsName 0x179A8627…                   gregskril.eth  (skim 904.0 ms)
all match
```
- **Every answer matches,** including the `ResolverNotFound` reverts (`0x77209fe8`), byte for byte.
- **Names on the warm path answer in single-digit milliseconds.**
- **A cold name takes one lookup to fetch** (`taek.eth` 286 ms, `gregskril.eth` 1.6 s, its reverse record 0.9 s). The second call for the same name is local (`UR.resolve gregskril.eth`: 1.4 ms).
