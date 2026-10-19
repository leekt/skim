# skim compared: Helios, a full node, an archive node

Four ways to read Ethereum without taking a third party's word for it:

- **[Helios](https://github.com/a16z/helios) with an RPC provider:** a light client in front of a provider. It follows the beacon chain's sync committee for headers, and checks each answer's Merkle proofs against the verified state root. An `eth_call` asks the provider for an access list (`eth_createAccessList`), fetches a proof per account touched (`eth_getProof`), and runs the code locally ([a16z: Building Helios](https://a16zcrypto.com/posts/article/building-helios-ethereum-light-client/)). It needs two external endpoints: an execution RPC with `eth_getProof`, and a consensus endpoint serving the light-client beacon API.
- **skim:** a peer on the network that keeps only what your app uses. It follows the chain through gossip, verifies each block's state changes against its BAL commitment, keeps subscribed state in memory, and fetches anything else from peers over SNAP with proofs.
- **A full node** (an execution client such as geth, reth or Nethermind, plus a consensus client such as Lighthouse): it downloads and executes every block itself and holds the whole current state, plus recent states (pruned).
- **An archive node:** a full node that also keeps every historical state, so any account or slot can be read at any past block.

The verification scopes differ. skim always checks fetched state proofs and block commitments; its default `latest` follows optimistic gossip, while `safe`, `finalized` and `--require-verified` need a bootstrapped light client and usable signed updates. A full or archive node also re-executes every block. See [trust and correctness](trust-and-correctness.md).

**Where the numbers come from:**
- **skim:** measured (the [M7 experiment log](../experiments/m7-gates-and-acquisition.md), [hardware requirements](hardware.md)).
- **Full node on Sepolia:** measured on this project's development machine: Nethermind (default Sepolia config, a pruned full node) with Lighthouse.
- **Mainnet nodes:** from the clients' published requirements ([reth](https://reth.rs/run/system-requirements/), as of June 2025; [geth](https://geth.ethereum.org/docs/getting-started/hardware-requirements)).
- **Helios:** from its documentation and design, not measured here.

Use them as orders of magnitude, and measure your own workload.

**Network scope:** skim needs Glamsterdam (BALs, eth/71, Gloas payload envelopes). Sepolia is built in; `skim run --network <dir>` also accepts compatible configured networks using the `mainnet` or `minimal` consensus preset, with a minimal-preset devnet verified end to end. A preset is not a built-in mainnet configuration. See [network configuration](configuration.md#networks). Helios and full/archive clients have their own supported-network lists; the comparisons below use the cited versions and measurements.

## At a glance

| | Helios + RPC provider | skim | Full node (EL + CL) | Archive node |
|---|---|---|---|---|
| Who serves data | your provider and a beacon light-client endpoint | Ethereum peers | itself | itself |
| Request cost | per request: an access list plus a proof per account for each `eth_call` | none | none | none |
| CPU | low | 0.25-0.5 core | 4 cores recommended (geth); Nethermind ran ~8.5 cores of background load on Sepolia here | similar, more during sync |
| Memory | small | short macOS soaks ~100 MB median; Linux 179 MB at 26 min, 276 MB at 3 h with an indexer | 8-16 GB+ | 16 GB+ |
| Disk | none | ~1 MB head-only; ~140 MB with `--history local` | Sepolia: 345 GB measured (332 EL + 13 CL). Mainnet: 1.2 TB+ (reth), 650 GB+ growing ~14 GB/week (geth), 2 TB SSD recommended | mainnet: 2.8 TB+ (reth); 12 TB+ for geth's full archive |
| Bandwidth | proportional to requests | ~0.3 MB/s in, ~0.1 MB/s out, steady | 24-25 Mbit/s+ recommended; the whole chain on first sync | the same |
| Startup | seconds (from a checkpoint) | following in ~15 s; new keys wait on peers | sync: hours to days | longer (all history) |
| Reads of current state | provider round trips per call | ms when held; peers when not | local: any account, ms | local: any account, ms |
| Reads at past blocks | recent blocks only (what the provider proves) | the in-memory window; archived keys since subscribed (~2 weeks back with backfill) | recent states only (pruned) | any block |
| Logs and receipts | through the provider | since Glamsterdam, from peers (and stored matches) | the chain's history the client keeps | all |
| Throughput | the provider's rate limits | ~21k `eth_call`/s warm (dev machine) | Nethermind: 16.8k `eth_call`/s on the same machine | similar to a full node for current state |
| Fails when | the provider or the consensus endpoint is down, rate-limits or withholds | no peer serves what's needed | your machine is down or behind | the same |
| Privacy | the provider sees every address and call | requests spread across peers | nothing leaves your machine except p2p traffic | the same |
| Runs in | a browser, a phone, a server | a small server or desktop | a server with fast NVMe | a large server |

## Cost

**Helios:** the bill is the RPC provider's.
- **Each verified `eth_call`** needs one `eth_createAccessList`, then an `eth_getProof` per account touched (slots are proven in the same request as their account), plus the code of contracts not seen yet. A call through a router and a couple of pools touches several accounts: on the order of 5-10 provider requests. Helios caches proofs it has already verified.
- **Simple reads** (`eth_getBalance`, `eth_getStorageAt`, `eth_getCode`, `eth_getTransactionCount`) need one `eth_getProof` each.
- **Pricing:** providers bill per request or per weighted unit, and proof and access-list methods tend to weigh more than plain reads. Multiply requests per call by calls per day, at your provider's prices.
- **Cost scales with traffic.** Running this project's soaks against a provider for one day ended in HTTP 429 "Too Many Requests" on every request.

**skim:** no per-request cost; a small machine and its bandwidth.
- **Machine:** 1 core / 512 MB for skim alone; 2 cores / 2 GB with an indexer and Grafana ([hardware](hardware.md)).
- **Traffic:** the measured background traffic was ~33 GB/day (in and out). Additional cold reads, scans and serving traffic can increase it.

**Full node:** no per-request cost; a much bigger machine.
- **Machine:** 4+ cores, 16 GB RAM and a 2 TB NVMe SSD for mainnet (geth's recommendation, consensus client included), or 1.2 TB+ and 8 GB+ (reth). Fast storage matters: slow SSDs and HDDs struggle to keep up.
- **Disk keeps growing,** about 14 GB/week for geth until pruned.

**Archive node:** the same, with 2.8-12 TB+ of fast storage. Storage is most of the bill.

**Break-even:**
- **Low, bursty reads:** Helios.
- **Sustained reads of a known working set:** skim, the cheapest machine with no request bill.
- **Every contract's current state with uniform low latency, or full history:** a full node.
- **Arbitrary past state:** an archive node.

## Hardware

| | Helios | skim | Full node | Archive node |
|---|---|---|---|---|
| CPU | per request (proof checks, local execution) | 0.25-0.5 core steady (verifying blocks, keeping subscribed state current, serving peers); bursts during catch-up | executes every block; several cores, heavy during sync | the same, more during sync |
| Memory | small | workload/uptime dependent: ~100 MB median in short macOS soaks, up to 276 MB over a measured 3 h Linux run; counters explain part of RSS | 8-16 GB+ (more helps caching under RPC load) | 16 GB+ |
| Bandwidth | proofs per request plus sync-committee updates | ~0.3 MB/s in, ~0.1 MB/s out | 24-25 Mbit/s+; the whole chain first, then block and transaction gossip | the same |
| Storage | none | restart snapshot ~1 MB; history ~140 MB on Sepolia | Sepolia 345 GB; mainnet 650 GB-2 TB, NVMe | mainnet 2.8-12 TB+, NVMe |

## Usability

**Latency:**
- **Helios:** every call pays provider round trips (the access list, then the proofs in parallel), a few hundred milliseconds over the internet.
- **skim:**
  - State it holds (subscribed or cached) answers in milliseconds, and each block's subscribed state is ready ~0.9 s (median) after its slot starts.
  - State it doesn't hold has to come from peers: seconds, or longer when SNAP peers are scarce.
- **Full and archive nodes:** any state, local disk, milliseconds. No cold-read penalty beyond the disk, which is why fast NVMe matters.

**Startup:**
- **Helios:** seconds from a checkpoint (at most two weeks old).
- **skim:** follows the chain within ~15 s of a restart and replays its restart snapshot in seconds. Keys it doesn't hold wait for a SNAP peer that serves the head: restarts were ready in 16-145 s in soaks, and a few waited minutes.
- **Full node:** an initial sync of hours to days. A restart resumes in minutes, but a node that was off for a while must catch up before it answers about the head.
- **Archive node:** longest of all to sync, since it keeps every historical state.

**Downtime:**
- **Helios:** shares the provider's. An outage, a rate limit or a revoked key stops every request. A dishonest provider can withhold data, not falsify it.
- **skim:** no single point of failure in the data path, but it depends on peers. On Sepolia, SNAP peers that serve the head are the scarce kind, and a fresh node can take minutes to get usable execution peers.
- **Full and archive nodes:** depend only on your machine and enough peers to follow the chain. Disk failures, full disks and forgotten upgrades at forks are the usual causes.

**What you can ask:**
- **Helios:** the standard methods it can verify.
- **skim:**
  - the standard methods, for what it holds or can fetch;
  - subscriptions that keep state warm, and per-block call results;
  - block streams for indexers;
  - logs since Glamsterdam.
- **Full node:** everything about current and recent state, plus the history the client keeps (clients increasingly drop old pre-merge history by default).
- **Archive node:** everything, at every block.

**Maturity:**
- **Helios:** its README notes it hasn't been audited.
- **skim:** experimental; Sepolia plus configured Glamsterdam networks, including a verified minimal-preset devnet. Public-peer verification and data availability must be checked per deployment.
- **Full and archive nodes:** the mature, battle-tested option.

## Wallet or user, or app developer

**A wallet, or one user:**
- **Helios fits best today.** It runs inside the wallet (WebAssembly, mobile), starts in seconds, needs no machine and works on mainnet; at wallet volume the provider costs little. In exchange, the provider sees your addresses and calls, and can stop answering.
- **skim:** for a user with an always-on machine who wants no provider. Point the wallet at it. Or a community-run skim serving many users.
- **A full node:** the most independent, but a 2 TB NVMe machine is a lot to ask of a user. People who already run a home staking node have one and can point their wallet at it.
- **An archive node:** more than a wallet ever needs.

**An app developer or backend:**
- **skim fits when the app's working set is known and history since the fork is enough:**
  - no request bill and no rate limit;
  - millisecond reads for the state you subscribe;
  - block streams and log history for indexing;
  - a machine a few times smaller than a full node's.
- **A full node fits when reads are unpredictable and must all be fast** (a block explorer, an RPC service, wallets for many users), or when you need the whole chain's recent history.
- **An archive node fits** when you need any state at any past block: analytics, accounting across many accounts, indexers that start at genesis, tracing old transactions.
- **Helios:** fine for occasional verified reads in a backend, but every call becomes several provider requests.

## Owning the infrastructure, or relying on someone else's

**Relying on a provider (Helios):**
- **Pros:** nothing to operate. The provider runs the full nodes, handles upgrades and scales, while proofs keep integrity yours.
- **Cons:** availability, rate limits, pricing, terms and privacy are the provider's. There are two external dependencies: the execution RPC and the consensus light-client endpoint (Helios's default is run by its maintainers).

**Owning a full or archive node:**
- **Pros:** complete independence and complete data. Nothing external but the p2p network.
- **Cons:** the heaviest operations. Terabytes of fast storage that keep growing, days to sync, an execution and a consensus client to keep upgraded for every fork, and disk and memory to watch.

**Owning skim:**
- **Pros:** no provider and no full node. A small machine, flat cost, nobody between you and the network.
- **Cons:** you operate it (keep it running, upgrade it for forks, watch it with Grafana or `skim_status`), and it depends on what peers serve: cold state and history come from them, and on Sepolia SNAP peers serving the head can be scarce.
- **Scope:** it holds what you use, not the chain, so it doesn't replace a full node for apps that need everything.

**Combining them is common:**
- skim for your app's own sustained traffic, plus Helios (or skim's JSON-RPC behind your service) for users' occasional verified reads;
- skim in front of a full node you already run, keeping the hot working set in memory;
- an archive node only for analytics jobs, with skim or a full node serving live traffic.
