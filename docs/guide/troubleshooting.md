# Troubleshooting

### Slow first calls, or `-32002` "no SNAP state peers connected"
SNAP-serving EL peers take **1–4 minutes** to connect on public Sepolia after a cold start. Consensus gossip makes the head ready within seconds, so skim already answers from subscribed state.

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
- **Not included yet:** the receipt is `null` until a block includes the transaction. skim re-broadcasts every 12 s for 30 minutes.
- **Dropped by peers:** a fee below the current base fee, or a nonce gap (an earlier nonce still missing), makes peers ignore the transaction. Check `eth_getTransactionCount(addr, "pending")` against your nonce.
- **No EL peers:** check `skim_el: session` lines in the log.

### `-32004` from a tool
Some tools call methods skim doesn't serve, such as filters (`eth_newFilter`) or `eth_sendTransaction` with node-held keys. Use client-side signing (viem `privateKeyToAccount`, Foundry `--private-key`), and polling instead of filters (viem's `watch*` helpers poll by default over HTTP).

### Collecting a useful report
```bash
RUST_LOG=info,skim::rpc=debug,skim::chaindata=debug skim run --config skim.yaml 2> skim.log
```
`eth_call fetches` lines show predicted keys, rounds and wall time per call. `SNAP answers rejected by verification` counts proofs that failed (expected to stay at 0 or near it).
