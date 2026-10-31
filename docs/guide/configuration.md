# Configuration reference

skim reads one YAML file (`skim run --config skim.yaml`) plus command-line flags. Every YAML field is optional, and with no config skim serves state on demand. Select the chain with `--network`; a `network:` field in the app YAML does not select it.

## `skim.yaml`
```yaml
state:
  subscriptions:                      # your app's state: always kept ready
    - name: <unique name>             # kept current at the head; history only for --window blocks
      address: "0x…"                  # needed for account/storage
      account: true                   # balance, nonce, code hash (and the code)
      storage: ["0x0", "0x4"]         # explicit slots (hex strings), or "all" (whole storage, see below)
      max_slots: 50000                # storage: all only: refuse bigger contracts ("unlimited" allowed)
      calls:                          # simulations whose access sets stay ready
        - to: "0x…"
          data: "0x…"                 # full calldata
          from: "0x…"                 # optional caller (default 0x0)
      on_block: false                 # true: retain each block's call outputs in skim_subscriptionStatus
      retention: { mode: archive }    # keep every version on disk (needs --archive)

  on_demand:
    enabled: true                     # false: still fetched for correctness, kept 1 block only
    cache_ttl_blocks: 4               # unsubscribed state stays current this many blocks after use
    max_fetch_rounds: 8               # bounded resolve rounds per call; deep call chains may need more

  limits:
    max_subscribed_keys: 100000       # hard cap on subscribed keys in the store (see below); or "unlimited"
    max_on_demand_keys: 200000        # hard cap on on-demand keys; soonest-expiring evicted first; or "unlimited"

predictors:
  uniswap_v3:                         # input-aware prefetch for QuoterV2.quoteExactInputSingle
    pools: ["0x…", "0x…"]             # V3 pools (fee/tokens/tickSpacing are read on start)
    words: 2                          # tick-bitmap words kept warm on each side (grows with observed reach)
    prefetch_budget: 256              # background keys per pool per block (low priority)
    window_ttl_blocks: 16             # prefetched keys expire this long after leaving the window
```

### Subscriptions in detail
A subscription keeps its keys **current**: every block's access list updates them, so reads at the head never wait on a peer, and they never expire the way on-demand keys do. It doesn't keep their **history**: like every key, a subscribed key holds versions only for the last `--window` blocks (256 by default), and older versions fold into one. Removing a subscription stops the updates. Values at older blocks need `retention: { mode: archive }` (below, and [archive](archive.md)).

| Field | Effect |
|---|---|
| `account: true` | Subscribes balance, nonce and code hash. Code is cached for tracked accounts. |
| `storage: [slots]` | Subscribes exactly these slots. Use `cast index` / `cast storage` to find mapping slots, or subscribe a call instead. |
| `storage: all` | Downloads storage over SNAP, verifies the complete storage root, then tracks every BAL write to that contract. Slots absent from the verified trie are proven zero. `max_slots` defaults to 50,000 and also counts toward `max_subscribed_keys`. Cannot be combined with archive retention. |
| `calls` | Each call runs every block (local, ~ms). Every key it reads becomes a required key of the subscription, so the call itself and any call reading the same state stay warm. |
| `on_block: true` | Keeps each call's latest output (hex) or error in `skim_subscriptionStatus.results`. |
| `retention: { mode: archive }` | Stores every BAL-written version of this subscription's keys (including learned keys) in the `--archive` file, so historical `eth_getStorageAt` / `eth_call` work for them. See [archive](archive.md). |

**Whole-storage limits:** `max_slots` accepts `unlimited`, but memory then grows with the contract: roughly 100 bytes per slot is an estimate, so 10 million slots are on the order of 1 GB before other costs. Set `max_subscribed_keys` high enough too. The download must finish while a peer still serves its starting root; otherwise it retries from a newer root. A reorg below the download block discards the complete-storage view and triggers a fresh download. Later contract growth also counts toward the limits.

**Readiness** (`skim_subscriptionStatus`): `ready: true` requires all of the following:
- the head block's BAL is applied
- every required key is known at the head (`missing_keys` counts the rest)
- for simulation subscriptions, every call ran at that block without error or unresolved misses (`calls_ok`)
- the key limit cut nothing (`truncated: false`)

**`max_subscribed_keys` is enforced where keys enter the store, not just counted.**
- **Startup:** declared keys over the limit fail at startup.
- **Learned keys:** a simulation's learned keys are subscribed only while there is room. The rest stay on-demand, with a TTL, and the subscription reports `truncated: true`.
- **Background prefetch:** the V3 tick window uses on-demand TTL retention and its budget; actual keys learned by call subscriptions count toward the subscribed-key limit.
- **Dropped keys:** keys a call stops reading are unsubscribed. Archived ones are released only after no subscription has needed them for 64 blocks (so a key read now and then doesn't flap). Their archived history stays readable up to the release, and reads after it are refused. If a call reads the key again, its history starts afresh; the gap is never bridged. The same applies to archived keys of a subscription removed from the config.
- **Bytecode:** code is dropped when no tracked account points at it any more.

### Choosing between slots and calls
- **You know the layout** (an ERC-20 balance, a V3 `slot0`): list slots. It's the cheapest option.
- **You know the call your app makes** (a quote, `getReserves`, a lens contract): subscribe the call. This is the right default for anything non-trivial, including Uniswap V4, whose pools live inside one `PoolManager`.

## Networks

`skim run` defaults to the built-in Sepolia configuration. **Hoodi is built in too** (`--network hoodi`): its published metadata (eth-clients/hoodi), mainnet preset, Glamsterdam at epoch 132352 (2026-10-26 17:42:48 UTC), default checkpoint `checkpoint-sync.hoodi.ethpandaops.io`. Before a network's Glamsterdam fork skim can't follow it (there are no BALs): it says so at startup, `skim status` shows the time left, and it picks the chain up at activation.

To follow another Glamsterdam network, pass a network-config directory (an eth-clients `metadata/` directory works as is: bootnodes may be `.txt` or `.yaml` lists):

```bash
skim run --network /path/to/network --config skim.yaml \
  --checkpoint-url http://127.0.0.1:5052 --data-dir data/my-network
```

The directory supplies:

| File | Required contents |
|---|---|
| `config.yaml` | `GLOAS_FORK_VERSION`, `GLOAS_FORK_EPOCH`, slot duration (`SLOT_DURATION_MS` or `SECONDS_PER_SLOT`), and `PRESET_BASE` (`mainnet` or `minimal`; defaults to `mainnet`). `BLOB_SCHEDULE` controls scheduled blob-parameter fork digests. |
| `genesis.ssz` or `genesis_validators_root.txt` | Beacon genesis time and validators root from the SSZ state, or a 32-byte hex root in the text file. Without SSZ, time comes from `MIN_GENESIS_TIME + GENESIS_DELAY` in `config.yaml`. |
| `genesis.json` | Execution genesis, including chain id and `amsterdamTime`. Cancun (`cancunTime`) must be active by `amsterdamTime`, or skim refuses the network: it applies BAL post-values as the new state, which is complete only with EIP-6780's SELFDESTRUCT rules. |
| `bootstrap_nodes.txt` (optional) | CL discovery ENRs. Direct peers can also be supplied with `--cl-peer`. |
| `enodes.txt` (optional) | EL bootnodes, one `enode://` per line; `--el-bootnode` adds more. |

skim dials the nodes in `bootstrap_nodes.txt` and `enodes.txt` directly as well as through discovery: a devnet's listed nodes are its clients, and they may not answer discovery.

The `mainnet` preset names consensus constants; it is not a built-in `--network mainnet` option. The network must provide post-Glamsterdam payloads and BALs. Fork digests follow the configured blob schedule: Status carries the current fork digest, and gossip topics overlap two epochs either side of a transition. A live blob-parameter fork crossing has been verified on the devnet. The EL fork id also covers forks scheduled after Amsterdam. A scheduled fork skim doesn't implement (Hegota without `--experimental-hegota`, or any fork it doesn't know) is a stop: skim applies no block from it on, and refuses to start if it's already active ([Hegota](hegota.md)).

Consensus bootstrap uses `--checkpoint`, then a saved verified finalized root, then `--checkpoint-url`. Only built-in Sepolia supplies a default checkpoint URL. With no checkpoint source on another network, the node follows gossip optimistically and records the bootstrap error in `skim_status.light_client`; `--require-verified` refuses `latest` until verification succeeds. See the [devnet example](../../devnet/README.md) and [trust guide](trust-and-correctness.md).

## `skim run` flags
`skim` with no subcommand (or with only these flags) is `skim run`: with no config at all it follows Sepolia, serves JSON-RPC on `127.0.0.1:8545`, and fetches every state read on demand. `skim status` asks a running node how it's doing (chain, peers, light client, subscriptions) and says what to do about problems.

Only `skim run` follows networks other than Sepolia (`--network`). The other subcommands (`probe-*`, including `probe-retention`, `follow`, `bench-node`) are diagnostics and measurement tools from the experiments and are Sepolia-only.

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | none | the YAML above |
| `--network <sepolia\|dir>` | `sepolia` | the chain to follow: `sepolia` (built in), or a network-config directory as consensus clients use (`config.yaml` with its `PRESET_BASE` (`mainnet` or `minimal`), `genesis.ssz` or `genesis_validators_root.txt`, `bootstrap_nodes.txt`) plus the execution `genesis.json` (with `amsterdamTime`) and optionally `enodes.txt`. Chain id, fork schedule, preset, slot duration and bootnodes all come from it; the data dir defaults to `~/.skim/<network>` |
| `--experimental-hegota` | off | follow Hegota (Heze + Bogotá) when the network schedules it, under assumptions that will change with its spec: Glamsterdam's EVM rules, the drafted Heze block layout, frame transactions decoded but not executed. Off: skim stops applying blocks at Hegota ([Hegota](hegota.md)) |
| `--el-bootnode <enode>` | none | extra EL bootnodes, for networks without a DNS tree or `enodes.txt` (repeatable) |
| `--cl-peer <multiaddr>` | none | CL peers to dial directly and keep (`/ip4/…/tcp/…/p2p/<peer id>`), e.g. a devnet's beacon nodes (repeatable) |
| `--hub <url>` | none | attach to a skim-hub: **not supported yet**, skim refuses to start with it |
| `--rpc <addr>` | `127.0.0.1:8545` | JSON-RPC listen address (HTTP and WebSocket; single and batch requests) |
| `--cors <origin\|*\|none>` | `*` | `Access-Control-Allow-Origin` for browser pages calling skim directly. `*` lets any page in this machine's browser call the standard methods; skim holds no keys and listens on localhost by default. `skim_*` methods (and the `skim_subscriptions` WebSocket subscription) reveal what the node watches, so a page gets them only from an origin named here exactly, never through `*`. WebSocket upgrades from a page are checked against this too (`403` for an origin it doesn't allow). Requests without an `Origin` header (curl, servers, Node) are not affected. Set an origin, or `none`, if the RPC is exposed beyond this machine |
| `--cl-port <port>` | `9130` | consensus-layer libp2p TCP + discv5 UDP; QUIC on port+1 |
| `--el-port <port>` | `30388` | execution-layer RLPx TCP + discovery UDP |
| `--window <blocks>` | `256` | in-memory history for reads at recent blocks and reorgs |
| `--archive <path>` | off | on-disk archive (`redb`) for `retention: archive` subscriptions |
| `--archive-backfill <blocks>` | `0` | extend archived history backwards from retained BALs (~2 weeks) |
| `--history <off\|local>` | `off` | `off`: in-memory chain-data history; `local`: post-fork headers and fetched matching logs in `history.redb`; requires a data dir and cannot be combined with `--ephemeral` |
| `--max-logs-range <blocks>` | `2048` | largest range one `eth_getLogs` may scan |
| `--tx-index <subscribed\|all>` | `subscribed` | which transactions `eth_getTransactionByHash`, `eth_getTransactionReceipt` and `debug_traceTransaction` can find by hash. `subscribed`: only transactions whose BAL changes touch a subscribed address (an address with any subscribed key), which includes everything sent from one (its nonce changes). Nothing is kept about other transactions; lookups for them return `null`. A transaction that only *reads* a subscribed contract isn't indexed (BAL reads carry no transaction position). `all`: every transaction, e.g. for a wallet backend or explorer. Transactions sent through skim are always found (see [JSON-RPC](json-rpc.md)) |
| `--tx-index-blocks <blocks>` | `2048` | recent blocks the transaction index covers (2,048 is ~7 h of 12 s blocks); `0` = none (sent transactions are still tracked) |
| `--exclude-snap-peer <node-id>` | none | never use this peer for state (e.g. a node you benchmark against); repeatable |
| `--data-dir <path>` | `~/.skim/<network>` | restart data: snapshot of subscribed state, EL node key, known peers and verified finalized root; built-in Sepolia uses `~/.skim/sepolia`, custom networks use the directory's basename |
| `--ephemeral` | off | disable restart persistence and use a new node identity; incompatible with `--history local`. An explicitly supplied `--archive` still writes its archive file |
| `--trusted-peer <enode>` | none | EL peer to keep connected, e.g. your own node (answers are still proof-checked); repeatable |
| `--cl-mesh <n>` / `--cl-peers <n>` | `8` / `50` | consensus gossip mesh degree and peer target |
| `--checkpoint <root>` | none | trusted beacon block root for the light client (default: last finalized root verified in a previous run, else `--checkpoint-url`) |
| `--checkpoint-url <url>` | ethPandaOps (Sepolia); none otherwise | checkpoint-sync endpoint (or a beacon node's API) for the light client's starting root, used if no explicit or saved checkpoint is available |
| `--require-verified` | off | `latest` = highest block the sync committee signed off on (~1–2 slots behind) |
| `--no-call-cache` | off | execute every `eth_call` instead of caching results by block hash and request |
| `--metrics <addr>` | off | serve Prometheus metrics at `http://<addr>/metrics` (see [metrics/](../../metrics)) |
| `--metrics-addresses <n>` | `50` | most-called addresses exported with per-address series; `0` exports none |
| `--metrics-idle <s>` | `300` | drop series that stopped updating (peers that left, addresses that fell out of the top) |
| `--state-wait <s>` | `60` | how long a request needing unheld state waits for a SNAP peer when none is connected (first minutes after a cold start) |
| `--no-serve` | off | don't answer peers' requests for headers, bodies, receipts, BALs, bytecode and payload envelopes (they get empty answers) |
| `--no-learned-patterns` | off | disable the similar-input learned tier (for A/B measurements) |

**Logging:** set `RUST_LOG`, e.g. `RUST_LOG=info,skim::rpc=debug`. That logs every RPC with its latency, and each `eth_call`'s fetch waves.

## Restarts
The snapshot is a cache, never a source of truth. On start it is used only after:
1. headers from its block to the current head link by hash
2. every block's access list matches its header
3. those blocks' writes are replayed

Subscriptions don't wait for the replay: they fetch what they need right away, and the replay fills in only what is still unknown when it finishes. Restored values are provisional: they are cached like on-demand state until the first subscription pass re-subscribes the keys the current config still needs. Keys of removed subscriptions expire instead of staying subscribed.

Anything that doesn't check out (a reorg while skim was down, a gap over 8,192 blocks, an unreadable file) is discarded, and state is fetched as on a fresh start. The replay needs one EL peer that serves eth/71. In practice, restart time is dominated by how long the first such peer takes to connect.

## Sizing
- **Memory:** retained state and caches have separate limits.
  - **Subscribed and on-demand keys:** both have hard caps (`limits`).
  - **Contract code:** freed with its accounts.
  - **The recent-BAL index:** every write of the last `--window` blocks, needed to replay fetched values to the head. This part scales with chain activity, not with your keys. `skim_status.store.recent_bal_bytes` shows it; lower `--window` to shrink it.
  - **Block data and results:** block-count limits on headers, transaction indexes, bodies, receipts and BALs; the call cache clears before a new insert when it already holds 20,000 entries or an estimated 32 MiB. A single result can take it over the byte threshold until the next insertion.
  - **Database caches and peers:** bounded page caches and EL session targets also consume memory; network buffers and allocator overhead are not fully counted by `skim_status.memory`.

  There is no single total RSS cap. Measurements (Linux aarch64, `--history local` plus an indexer, default window): 179 MB after 26 minutes; on a build that indexed every transaction, 281 MB at 3 h and 346 MB at 9 h, flat after ~7 h once the caches were full. Short macOS soaks with `--window 32` had 99–105 MB medians. These are different workloads and durations, not upper bounds; a run on the default `--tx-index subscribed` build is still being measured. See [hardware requirements](hardware.md) and [memory and latency](memory-and-latency.md).
- **Network and CPU:** skim receives and checks every block's full payload and BAL, so this part follows the whole chain's activity, not just your keys. What scales with your app is the state it keeps and the fetches it makes.
- **Disk:** restart data in `--data-dir` by default; `--history local` adds `history.redb`, and `--archive` writes its separately selected file. `--ephemeral` without `--archive` keeps no persistent state. Use a separate data directory and archive for each chain/genesis.
