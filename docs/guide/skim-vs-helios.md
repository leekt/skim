# skim or Helios with an RPC provider

Both give you verified Ethereum reads without trusting whoever serves the data. They get there differently:

- **[Helios](https://github.com/a16z/helios)** is a light client in front of an RPC provider. It follows the beacon chain's sync committee for headers, and for each request asks the provider for data plus Merkle proofs, which it checks against the verified state root. An `eth_call` asks the provider for an access list (`eth_createAccessList`), fetches a proof for each account it touches (`eth_getProof`), and runs the code locally ([a16z: Building Helios](https://a16zcrypto.com/posts/article/building-helios-ethereum-light-client/)). It needs two external endpoints: an execution RPC that supports `eth_getProof`, and a consensus endpoint serving the light-client beacon API.
- **skim** is a peer on the network. It follows the chain through gossip, verifies each block's state changes against its BAL commitment, keeps the state your app uses in memory, and fetches anything else from peers over SNAP with proofs. No provider, no API key.

Both verify what they return and refuse rather than guess. What differs is who serves the data, what each request costs, and where the work happens.

**Numbers on this page:** skim's are measured (the [M7 experiment log](../experiments/m7-gates-and-acquisition.md), [hardware requirements](hardware.md)). Helios's are from its design and documentation, not measured here: use them as orders of magnitude, and measure with your own workload and provider.

**Networks today:** skim needs Glamsterdam (BALs, eth/71, payload envelopes), so it runs on **Sepolia only** until mainnet forks. Helios supports mainnet, Sepolia, Hoodi, OP Stack chains (OP Mainnet, Base) and Linea now.

## At a glance

| | Helios + RPC provider | skim |
|---|---|---|
| Who serves data | your RPC provider (execution) and a beacon light-client endpoint (consensus) | Ethereum peers (EL and CL p2p) |
| RPC cost | per request: one access list plus a proof per account for each `eth_call` | none |
| CPU | low (verifies proofs, runs the call) | 0.25-0.5 core, steady |
| Memory | small ("lightweight enough to run on mobile devices") | ~100-180 MB following the head; ~250-300 MB in long catch-ups |
| Bandwidth | proportional to requests (proofs), plus light-client updates | ~0.3 MB/s in, ~0.1 MB/s out, steady (~26 GB/day in, ~7 GB/day out) |
| Storage | none | ~1 MB head-only; ~140 MB with `--history local` |
| Startup | seconds (from a checkpoint) | following the chain in ~15 s; state for new keys depends on peers (seconds to minutes) |
| Latency, state already held | network round trips to the provider for each call | milliseconds (memory) |
| Latency, cold | provider round trips (access list, then parallel proofs) | SNAP from peers: 40 cold calls on a fresh node took 6-53 s |
| Throughput | bounded by the provider's rate limits | ~21,000 `eth_call`/s on the development machine, warm state |
| Fails when | the provider is down, rate-limits you, or withholds data; the consensus endpoint is down | no peer serves what's needed (SNAP peers serving the head are the scarce kind) |
| Privacy | the provider sees every address and call | requests spread across peers; no account ties them together |
| Runs in | a browser (WebAssembly), a phone, a server | a server or desktop (a resident process) |

## Cost

**Helios:** the bill is the RPC provider's.
- **Each verified `eth_call`** needs one `eth_createAccessList`, then an `eth_getProof` per account touched (slots are proven in the same request as their account), plus the code of contracts not seen yet. A call through a router and a couple of pools touches several accounts: on the order of 5-10 provider requests. Helios caches proofs it has already verified, which helps when calls repeat.
- **Simple reads** (`eth_getBalance`, `eth_getStorageAt`, `eth_getCode`, `eth_getTransactionCount`) need one `eth_getProof` each.
- **Pricing:** providers bill per request or per weighted unit, and proof and access-list methods tend to weigh more than plain reads. Multiply requests per call by calls per day, at your provider's prices.
- **Cost scales with traffic.** A wallet doing a few reads per screen costs little. A backend doing thousands of calls per block hits paid tiers or rate limits. Running this project's soaks against a provider for one day ended in HTTP 429 "Too Many Requests" on every request.

**skim:** no per-request cost. You pay for the machine and its bandwidth:
- a 2-core / 2 GB instance runs skim with an indexer and Grafana ([hardware](hardware.md)); skim alone fits 1 core / 512 MB;
- ~33 GB/day of traffic (in and out), steady, whether you make one call or a million.

**Break-even:** skim's cost is flat, Helios's grows with requests. Low, bursty read volume favors Helios; sustained call volume (simulations, quotes every block, indexing) favors skim.

## Hardware

| | Helios | skim |
|---|---|---|
| CPU | proof checks and local execution per request; idle otherwise | 0.25-0.5 core all the time (verifying every block, keeping subscribed state current, serving peers); bursts during catch-up and cold fetches |
| Memory | small: verified headers, recent proofs | ~100 MB head-only with `--window 32`, ~180 MB with the default window on Linux; per-structure detail in `skim_status.memory` |
| Bandwidth | per request (proofs of a few KB per account) plus sync-committee updates | ~0.3 MB/s in, ~0.1 MB/s out, mostly block gossip and serving peers |
| Storage | none | restart snapshot ~1 MB; `--history local` ~140 MB on Sepolia after the fork; an archive if enabled |

Helios fits where skim can't: a browser extension, a phone, a serverless function. skim needs a process that stays up.

## Usability

- **Latency:**
  - **Helios:** every call pays provider round trips: the access list, then the proofs in parallel. Latency is the provider's, a few hundred milliseconds per call over the internet.
  - **skim, held state:** subscribed or cached state answers in milliseconds, and a block's subscribed state is ready about 0.9 s (median) after its slot starts.
  - **skim, unheld state:** has to be fetched from peers, which is slower than a provider when peers are few or busy.
- **Startup:**
  - **Helios:** syncs in seconds from a checkpoint (at most two weeks old).
  - **skim:** follows the chain within ~15 s of a restart, and replays its restart snapshot in seconds. Keys it doesn't hold need a SNAP peer that serves the head; in soaks, restarts were ready in 16-145 s, and a few waited minutes for such a peer.
- **Downtime:**
  - **Helios:** shares the provider's: an outage, a rate limit or a revoked key stops every request. A dishonest provider can withhold data, not falsify it.
  - **skim:** no single point of failure in the data path, but depends on the p2p network: on Sepolia, SNAP-serving peers are the scarce kind, and a fresh node can take minutes to get usable execution peers.
- **API coverage:**
  - **Helios:** the standard methods it can verify.
  - **skim:** the standard methods plus subscriptions that keep state warm, per-block subscription results, block streams for indexers (`skim_streamNext`), and logs with stored history (`--history local`).
- **Maturity:** both are young. Helios's README notes it hasn't been audited. skim is experimental and Sepolia-only.

## Wallet or user, or app developer

**A wallet, or one user:**
- **Helios fits better today.** Low volume, runs inside the wallet (WebAssembly, mobile), starts in seconds, no machine to run, and works on mainnet. The cost of a provider at wallet volume is small.
- **What it gives up:** the provider sees your addresses and calls, and can stop answering you.
- **skim:** worth it for a user who already runs something always-on (a home server, a desktop) and wants no provider at all. Point the wallet's RPC at it. Or a community-run skim serving many users, which removes the provider but adds trust in the operator's availability (not in correctness: the user can still verify with a light client).

**An app developer or backend:**
- **skim fits better** once traffic is sustained:
  - no per-request bill or rate limit;
  - millisecond reads for the state your app uses (subscribe it);
  - block streams and log history for indexing;
  - throughput that doesn't depend on a provider plan;
  - one predictable machine cost.
- **Helios:** fine for occasional verified reads in a backend, but every call becomes several provider requests, so a busy backend multiplies its provider bill and runs into rate limits.

## Owning the infrastructure, or relying on someone else's

**Relying on a provider (Helios):**
- **Pros:** nothing to operate; the provider runs full nodes, handles upgrades and scales; you keep integrity (proofs), so a bad provider can't make you accept wrong data.
- **Cons:** availability, rate limits, pricing and terms are the provider's. You also need a consensus light-client endpoint (Helios's default is run by its maintainers), so that's two external dependencies. Privacy: the provider sees your traffic.

**Owning it (skim):**
- **Pros:** no provider and no API key, flat cost, no rate limits, nobody between you and the network.
- **Cons:** you operate it. That means keeping it running, upgrading it for forks, watching it (Grafana, `skim_status`), and living with what the p2p network gives you (peers come and go; SNAP peers that serve the head can be scarce).
- **What owning means here:** skim is light enough to run on a small instance, not a full node. It doesn't store the chain; it keeps what you use.

**A middle path:** run skim for your app's own traffic (sustained, latency-sensitive), and give end users Helios (or skim's JSON-RPC behind your service) for the occasional verified read.
