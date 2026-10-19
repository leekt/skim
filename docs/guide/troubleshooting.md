# Troubleshooting

### Slow first calls, or `-32002` "no SNAP state peers connected"
SNAP-serving EL peers have taken **1–4 minutes** to connect on public Sepolia after a cold start. Consensus gossip can make the head state-ready within seconds, but subscriptions can still be missing keys; a ready head does not mean all subscribed state has been fetched.

State skim hasn't seen comes only from SNAP peers: gossip carries each block's writes, not untouched state. A request that needs such state **waits for a SNAP peer** (up to `--state-wait`, default 60 s), then fetches and answers. It fails with this error only if no peer arrives in time.

HTTP clients have their own timeouts (viem: 10 s by default), so the first calls after a cold start can time out on the client side. Retry, or raise the client timeout. Watch for log lines like:
```text
INFO skim_el: session peer=… client=Geth/… snap2=false
INFO skim_el: session peer=… client=Nethermind/… snap2=true
```
Subscribed state shows `ready: false` with `missing_keys > 0` in `skim_subscriptionStatus` until then. `skim_status.snap_peers` and `skim_status.snap_fetch` show the state peers and their fetch counters.

To get peers faster:
- **Open `--el-port`** (TCP + UDP).
- **Keep `--data-dir`** (the default; not `--ephemeral`). skim saves the peers it dialed and re-dials them on start, SNAP-capable ones first. Those peers are often full, though, so this sometimes saves minutes and sometimes doesn't.
- **Pin a peer you run** with `--trusted-peer`.

**What's happening with peers:** `skim_status.peer_acquisition` (or the Grafana "Peer acquisition" row) shows:
- time to the first SNAP peer
- SNAP-capable peers seen, and how many stayed at least 60 s
- sessions by client
- why sessions ended (`TooManyPeers` = the peer was full)

On Sepolia about half of the SNAP-capable peers drop new connections within a minute because they are full. skim saves the peers that stayed, so the next start dials them first.

### No `state ready` lines
skim isn't receiving consensus gossip. Check:
- **Outbound UDP and TCP to the CL port range.** Some networks block discv5 (UDP).
- **The clock.** Gossip validation is slot-based, so a skewed clock drops messages.
- **Two skims on the same ports.** Pass a different `--cl-port`/`--el-port` per instance.
- **Custom networks:** check the config/genesis and bootnodes selected by `--network`. Peers must match the Gloas/blob-parameter fork digest. skim dials QUIC first when a peer announces it, then TCP; over TCP it offers only yamux, so a TCP-only peer without yamux can't connect ([TODO 25](../TODO.md)). Use `--cl-peer` to add a known peer directly. See [network configuration](configuration.md#networks) and the [devnet walkthrough](../../devnet/README.md).

### Custom network: EL peers drop at the handshake
On a network with a fork scheduled after Amsterdam (for example a blob-parameter fork), builds before commit `a1ac222` computed the wrong EL fork id once that fork was active. Sessions opened before the fork survived; after it, peers closed them and refused new ones at the handshake. Update skim. A peer may keep refusing the old address for a while (on the devnet: `ECIES ... UnreadableStream`); starting skim from another address avoids the wait ([devnet walkthrough](../../devnet/README.md#state-edge-cases)). If peers still drop right after connecting, compare `genesis.json` with the one your peers use: the fork id is derived from its genesis hash and fork timestamps, so any difference (a different `amsterdamTime` or BPO time, for example) produces the same symptom.

skim also refuses at startup a `genesis.json` whose Cancun (`cancunTime`) isn't active by `amsterdamTime` ("Cancun (EIP-6780) must be active by amsterdamTime").

### `safe`, `finalized` or `--require-verified` stays unavailable

Inspect `skim_status.light_client`: `trusted_from`, `period`, `verified_block`, `finalized_block`, `updates_rejected` and `last_error`. The default checkpoint URL exists only for built-in Sepolia. Other networks need an explicit `--checkpoint`, a saved verified finalized root, or a reachable `--checkpoint-url`. The checkpoint must belong to that network and peers must serve compatible light-client data. Default gossip following can work while bootstrap fails; `--require-verified` keeps `latest` unavailable until an authenticated selected-chain block exists.

### Memory rises after startup

Compare RSS with `skim_status.memory.parts` or the Grafana retained-data panel. Block caches and the state window fill within about an hour on Sepolia; transaction indexes can fill for ~7 hours and headers for ~27. Counts, estimated payload bytes and reported capacities have different meanings, and peer buffer sizes/allocator slack are not fully counted. In [nine hours of production](hardware.md#memory) RSS levelled off once those caches were full. A rising RSS alone cannot identify a leak; note the process uptime and cache occupancy when reporting it.

### Every cold call takes seconds
Fetches go through whichever peers are connected. With 1–2 peers, a wide fetch (hundreds of keys, e.g. a very large swap's tick range) queues on them. Remedies, in order:
1. **Subscribe** the state or calls your app uses, so they're never cold.
2. For Uniswap V3, add `predictors.uniswap_v3.pools`. The tick window then stays warm, and new amounts fetch their range in one wave.
3. Give skim time. The peer pool grows over the first ~10 minutes.

### `eth_getLogs` is slow over long ranges
- **Receipts per candidate block:** for blocks from before skim started, only the header bloom can rule a block out, so busy filters (e.g. a popular topic with no address) match most blocks.
- **Mitigations:** narrow the address/topic filter, use shorter ranges, or keep skim running so more of the range has BAL data.
- **Range cap:** `--max-logs-range` caps the range; larger requests get `-32005`.

### `eth_sendRawTransaction` succeeded but no receipt
- **Not included yet:** the receipt is `null` until a block includes the transaction. skim re-broadcasts every 12 s for 30 minutes, then forgets it (`eth_getTransactionByHash` returns `null` from then on).
- **Dropped by peers:** a fee below the current base fee, or a nonce gap (an earlier nonce still missing), makes peers ignore the transaction. Check `eth_getTransactionCount(addr, "pending")` against your nonce.
- **No EL peers:** check `skim_el: session` lines in the log.

### `eth_getTransactionByHash` returns `null` for a mined transaction
skim indexes transactions by hash only for the last `--tx-index-blocks` blocks (2,048 by default), and with the default `--tx-index pinned` only those whose block access list shows a change to a pinned address (an address with any subscribed key, including the sender's nonce). A transaction that only reads a pinned contract, or touches nothing you subscribe, isn't indexed, so `eth_getTransactionByHash` and `eth_getTransactionReceipt` return `null` and `debug_traceTransaction` returns `-32002`. Remedies:
- **Look it up by block** if you know where it is: `eth_getBlockByNumber(n, true)`, `eth_getBlockReceipts`, `eth_getTransactionByBlockNumberAndIndex` and `debug_traceBlockByNumber` don't use the index.
- **Index everything** with `--tx-index all` (a wallet backend or explorer). Its memory grows with chain activity over the index window, not with your subscriptions. Blocks are indexed as they arrive, so only blocks after the restart are covered.
- **Subscribe the address** you care about, so its transactions are indexed from then on.

Transactions sent through skim are found whatever the mode, for 24 h after inclusion (at most 4,096 of them).

### `-32004` from a tool
Some tools call methods skim doesn't serve, such as filters (`eth_newFilter`) or `eth_sendTransaction` with node-held keys. Use client-side signing (viem `privateKeyToAccount`, Foundry `--private-key`), and polling instead of filters (viem's `watch*` helpers poll by default over HTTP).

### Collecting a useful report
```bash
RUST_LOG=info,skim::rpc=debug,skim_chain::chaindata=debug skim run --config skim.yaml 2> skim.log
```
`eth_call fetches` lines show predicted keys, rounds and wall time per call. `SNAP answers rejected by verification` counts proofs that failed (expected to stay at 0 or near it).
