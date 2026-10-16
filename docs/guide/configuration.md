# Configuration reference

skim reads one YAML file (`skim run --config skim.yaml`) plus command-line flags. Every field is optional, and with no config skim serves everything on demand.

## `skim.yaml`
```yaml
network: sepolia                      # the only network today

state:
  subscriptions:                      # your app's state: always kept ready
    - name: <unique name>
      address: "0x…"                  # needed for account/storage
      account: true                   # balance, nonce, code hash (and the code)
      storage: ["0x0", "0x4"]         # explicit slots (hex strings), or "all" (whole storage, see below)
      max_slots: 50000                # storage: all only: refuse bigger contracts ("unlimited" allowed)
      calls:                          # simulations whose access sets stay ready
        - to: "0x…"
          data: "0x…"                 # full calldata
          from: "0x…"                 # optional caller (default 0x0)
      on_block: false                 # re-run `calls` every block, results in skim_subscriptionStatus
      retention: { mode: archive }    # keep every version on disk (needs --archive)

  on_demand:
    enabled: true                     # false: still fetched for correctness, kept 1 block only
    cache_ttl_blocks: 4               # unsubscribed state stays current this many blocks after use

  limits:
    max_subscribed_keys: 100000       # hard cap on subscribed keys in the store (see below); or "unlimited"
    max_on_demand_keys: 200000        # hard cap on on-demand keys; soonest-expiring evicted first; or "unlimited"

predictors:
  uniswap_v3:                         # input-aware prefetch for QuoterV2.quoteExactInputSingle
    pools: ["0x…", "0x…"]             # V3 pools (fee/tokens/tickSpacing are read on start)
    words: 2                          # tick-bitmap words kept warm on each side (grows with observed reach)
    prefetch_budget: 256              # background keys per pool per block (low priority)
```

### Subscriptions in detail
| Field | Effect |
|---|---|
| `account: true` | Subscribes balance, nonce and code hash. Code is cached for tracked accounts. |
| `storage: [slots]` | Subscribes exactly these slots. Use `cast index` / `cast storage` to find mapping slots, or subscribe a call instead. |
| `storage: all` | Downloads the contract's entire storage over SNAP once, and accepts it only if every downloaded slot together rebuilds the account's proven storage root. The contract is then tracked by address, so every BAL write keeps it current, and any slot is answered locally. A slot absent from the verified trie is a proven zero. For contracts up to `max_slots` (default 50,000; `unlimited` removes the cap). The slots count toward `max_subscribed_keys`, so for a very large contract set that to `unlimited` (or high enough) too.

With no cap, memory grows with the contract: about 100 bytes per slot, so a contract with 10 million slots needs on the order of 1 GB. The download must also finish while peers still serve the root it started from (about 128 blocks); if it can't, skim retries from a newer root. Can't be combined with archive retention yet. A reorg below the download block discards it, and it is downloaded again. |
| `calls` | Each call runs every block (local, ~ms). Every key it reads becomes a required key of the subscription, so the call itself and any call reading the same state stay warm. |
| `on_block: true` | Keeps each call's latest output (hex) or error in `skim_subscriptionStatus.results`. |
| `retention: { mode: archive }` | Stores every BAL-written version of this subscription's keys (including learned keys) in the `--archive` file, so historical `eth_getStorageAt` / `eth_call` work for them. See [archive](archive.md). |

**Readiness** (`skim_subscriptionStatus`): `ready: true` requires all of the following:
- the head block's BAL is applied
- every required key is known at the head (`missing_keys` counts the rest)
- for simulation subscriptions, every call ran at that block without error or unresolved misses (`calls_ok`)
- the key limit cut nothing (`truncated: false`)

**`max_subscribed_keys` is enforced where keys enter the store, not just counted.**
- **Startup:** declared keys over the limit fail at startup.
- **Learned keys:** a simulation's learned keys are subscribed only while there is room. The rest stay on-demand, with a TTL, and the subscription reports `truncated: true`.
- **Background prefetch:** the V3 predictor's subscribed keys count toward the same limit.
- **Dropped keys:** keys a call stops reading are unsubscribed. Archived ones are the exception, because their history must stay continuous.
- **Bytecode:** code is dropped when no tracked account points at it any more.

### Choosing between slots and calls
- **You know the layout** (an ERC-20 balance, a V3 `slot0`): list slots. It's the cheapest option.
- **You know the call your app makes** (a quote, `getReserves`, a lens contract): subscribe the call. This is the right default for anything non-trivial, including Uniswap V4, whose pools live inside one `PoolManager`.

## `skim run` flags
| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | none | the YAML above |
| `--rpc <addr>` | `127.0.0.1:8545` | JSON-RPC listen address (HTTP; single and batch requests) |
| `--cl-port <port>` | `9130` | consensus-layer libp2p TCP + discv5 UDP; QUIC on port+1 |
| `--el-port <port>` | `30388` | execution-layer RLPx TCP + discovery UDP |
| `--window <blocks>` | `256` | in-memory history for reads at recent blocks and reorgs |
| `--archive <path>` | off | on-disk archive (`redb`) for `retention: archive` subscriptions |
| `--archive-backfill <blocks>` | `0` | extend archived history backwards from retained BALs (~2 weeks) |
| `--max-logs-range <blocks>` | `2048` | largest range one `eth_getLogs` may scan |
| `--exclude-snap-peer <node-id>` | none | never use this peer for state (e.g. a node you benchmark against); repeatable |
| `--data-dir <path>` | `~/.skim/sepolia` | restart data: snapshot of subscribed state, EL node key, known peers |
| `--ephemeral` | off | keep nothing on disk; every start begins empty with a new node identity |
| `--trusted-peer <enode>` | none | EL peer to keep connected, e.g. your own node (answers are still proof-checked); repeatable |
| `--cl-mesh <n>` / `--cl-peers <n>` | `8` / `50` | consensus gossip mesh degree and peer target |
| `--checkpoint <root>` | none | trusted beacon block root for the light client (default: last finalized root verified in a previous run, else `--checkpoint-url`) |
| `--checkpoint-url <url>` | ethpandaops Sepolia | checkpoint-sync endpoint for the light client's starting root |
| `--require-verified` | off | `latest` = highest block the sync committee signed off on (~1–2 slots behind) |
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

Anything that doesn't check out (a reorg while skim was down, a gap over 8,192 blocks, an unreadable file) is discarded, and state is fetched as on a fresh start. The replay needs one EL peer that serves eth/71. In practice, restart time is dominated by how long the first such peer takes to connect.

## Sizing
- **Memory:** bounded by three things.
  - **Subscribed and on-demand keys:** both have hard caps (`limits`).
  - **Contract code:** freed with its accounts.
  - **The recent-BAL index:** every write of the last `--window` blocks, needed to replay fetched values to the head. This part scales with chain activity, not with your keys. `skim_status.store.recent_bal_bytes` shows it; lower `--window` to shrink it.

  There is no single "total memory" cap. Measured: ~200 MB RSS for a few pools, tokens and quotes, following Sepolia with the default 256-block `--window` (~100 MB with `--window 32`).
- **Network and CPU:** skim receives and checks every block's full payload and BAL, so this part follows the whole chain's activity, not just your keys. What scales with your app is the state it keeps and the fetches it makes.
- **Disk:** none, unless `--archive`.
