# skim documentation

## Guide

1. [skim compared](guide/compared.md): skim, Helios, a full node and an archive node; costs, resources and guarantees
2. [What to set up for your app](guide/use-cases.md): choose subscriptions, on-demand reads, history and simulations for your workload
3. [Getting started](guide/getting-started.md): build, run, first call, subscriptions
4. [How skim works](guide/how-it-works.md): BALs, verified fetches, subscriptions, prediction
5. [Configuration reference](guide/configuration.md): `skim.yaml` and `skim run` flags
6. [JSON-RPC reference](guide/json-rpc.md): methods, block tags, subscriptions, block streams, errors
7. [Archiving history for your state](guide/archive.md): `retention: archive`, backfill
8. [Hardware requirements](guide/hardware.md): measured CPU, memory, disk and bandwidth; machine sizing
9. [Memory and latency](guide/memory-and-latency.md): tuning, per-structure memory counters and the reasons behind defaults
10. [Trust and correctness](guide/trust-and-correctness.md): what's verified, what isn't
11. [Troubleshooting](guide/troubleshooting.md)

**Examples:** [Uniswap V3 and V4, WebSocket subscriptions, sending transactions, bigname and smallname](../examples/README.md)

**Monitoring:** [Prometheus and Grafana setup](../metrics/README.md)

**Design:** [PROJECT.md](../PROJECT.md)

## Experiment logs

Every result in the README comes from one of these runs, on Sepolia over public peers.

| Doc | What was measured |
|---|---|
| [M0 spike A](experiments/m0-spike-a.md) | CL payload gossip: receiving `execution_payload` envelopes with BALs |
| [M0 spike B](experiments/m0-spike-b.md) | EL peers: eth/71 BALs, snap/2 point lookups, peer census |
| [M0 spike C](experiments/m0-spike-c.md) | cold-call SNAP rounds vs Infura `eth_call` |
| [M0 round 2](experiments/m0-round2.md) | learned access sets, k rounds per call |
| [M0 scheduler](experiments/m0-scheduler.md) | adaptive request scheduler, peer retention |
| [M0 soak](experiments/m0-soak.md) | 298k SNAP jobs, 0 failed |
| [M1 follower](experiments/m1-follower.md) | BAL application latency, state-ready time |
| [M2 miss tail](experiments/m2-p0-miss-tail.md) | prefetch, the predictor, priorities, peer removal |
| [M2 verified state](experiments/m2-verified-state.md) | Merkle-proof verification of every SNAP value |
| [M3 2-hour soak](experiments/m3-soak-2h.md) | subscriptions vs on-demand, 0 disagreements |
| [M4 archive](experiments/m4-archive.md) | selective archive, restart healing, BAL backfill |
| [M5 predictors and chain data](experiments/m5-predictors-and-chain-data.md) | learned patterns, V3 predictor in `skim run`, transactions/receipts/logs |
| [M6 correctness and bounds](experiments/m6-correctness-and-bounds.md) | review fixes, 2 h chaos soak (0 wrong), restart persistence, CL catch-up, light client status |
| [M7 gates and acquisition](experiments/m7-gates-and-acquisition.md) | regression gates, peer acquisition, restart recovery, memory and throughput |
| [P3 full-node baseline](experiments/p3-full-node-baseline.md) | same workload and block hashes as a full node: answers, latency, resources |

Tools used by these runs live in [`experiments/tools`](experiments/tools).
