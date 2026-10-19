# skim documentation

## Guide

1. [skim compared](guide/compared.md): skim, Helios, a full node and an archive node, BAL catch-up in full nodes, Geth's partial-state proposal and VOPS; costs, resources and guarantees
2. [What to set up for your app](guide/use-cases.md): choose subscriptions, on-demand reads, history and simulations for your workload
3. [Installation](guide/install.md): the install script, release binaries and checksums, building from source
4. [Getting started](guide/getting-started.md): zero-config start, first call, subscriptions
5. [How skim works](guide/how-it-works.md): BALs, verified fetches, subscriptions, prediction
6. [Configuration reference](guide/configuration.md): `skim.yaml`, `skim run` flags, configured Glamsterdam networks and consensus presets
7. [JSON-RPC reference](guide/json-rpc.md): methods, block tags, subscriptions, block streams, errors and their reasons
8. [Archiving history for your state](guide/archive.md): `retention: archive`, backfill
9. [Hardware requirements](guide/hardware.md): measured CPU, memory, disk and bandwidth; machine sizing
10. [Memory and latency](guide/memory-and-latency.md): tuning, per-structure memory counters and the reasons behind defaults
11. [Trust and correctness](guide/trust-and-correctness.md): what's verified, what isn't
12. [Troubleshooting](guide/troubleshooting.md): starting with `skim status`
13. [Future considerations](guide/future-considerations.md): state-root checks from a minimal witness, a validation-state mode (VOPS, AA-VOPS), embedding, peer privacy, RPC conformance

## Examples

[Examples overview](../examples/README.md) (viem, Sepolia contracts; `npm ci` in `examples/`):

| Example | Shows |
|---|---|
| [Uniswap V3](../examples/uniswap-v3) | pool state, quotes for any amount, a quote every block, `Swap` logs, readiness |
| [Uniswap V4](../examples/uniswap-v4) | `StateView` pool state, the V4 quoter, `PoolManager` logs filtered by pool id |
| [WebSocket](../examples/websocket) | `newHeads`, `logs` and `skim_subscriptions` pushes |
| [Sending a transaction](../examples/send-transaction) | sign locally, `eth_sendRawTransaction`, wait for the receipt |
| [bigname](../examples/bigname/README.md) | skim as the verified-lookup RPC for the bigname ENS indexer |
| [smallname](../examples/smallname/README.md) | an ENS API on skim with a small SQLite index for discovery and history (the reference indexer) |
| [devnet edge cases](../examples/devnet-edgecases.mjs) | state edge cases (creation, selfdestruct, slot clearing, EIP-7702) vs an executing node at the same block hash, on the [devnet](../devnet/README.md) |

## Operations and development

- **Monitoring:** [Prometheus and Grafana setup](../metrics/README.md); `metrics/gen_dashboard.py` generates the dashboard
- **Network testing:** [devnet walkthrough](../devnet/README.md): a local Glamsterdam devnet (minimal preset), `devnet/run.sh` and the differential check `devnet/check.py`
- **Development:** [implemented architecture](../PROJECT.md), [contributor handoff](HANDOFF.md), [open work](TODO.md) and [fuzzing](../fuzz/README.md)

## Experiment logs

These are historical measurements of the builds and workloads named in each log, primarily on Sepolia over public peers. Old commands, APIs and limitations belong to those runs; use the guides above and `skim run --help` for current behavior. The devnet's results are recorded separately in its walkthrough, and the later production memory curve is in [hardware requirements](guide/hardware.md#memory).

| Doc | What was measured |
|---|---|
| [M0 spike A](experiments/m0-spike-a.md) | CL payload gossip: receiving `execution_payload` envelopes with BALs |
| [M0 spike B](experiments/m0-spike-b.md) | EL peers: eth/71 BALs, snap/2 point lookups, peer census |
| [M0 spike C](experiments/m0-spike-c.md) | cold-call SNAP rounds vs a hosted RPC's `eth_call` |
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
| [Memory audit (2026-10-18)](experiments/memory-audit.md) | source audit of memory retention and the fixes applied; later production curves are in [hardware](guide/hardware.md#memory) |

### Experiment tools

Scripts used by these runs, in [`experiments/tools`](experiments/tools) (run with `python3 -I`; reference-node URLs are arguments or `REF_URL`):

| Tool | What it does |
|---|---|
| [`archive_check.py`](experiments/tools/archive_check.py) | historical reads from the selective archive vs a reference archive node (M4) |
| [`chaindata_check.py`](experiments/tools/chaindata_check.py) | transactions, receipts and logs vs a reference node |
| [`cold_read_bench.py`](experiments/tools/cold_read_bench.py) | cold reads of never-read keys, warm repeats and a contract call against a running skim, with failures by `error.data.reason` |
| [`chaos_soak.py`](experiments/tools/chaos_soak.py) | long correctness soak under restarts and key-set churn, compared at the same block hash |
| [`cl_vs_lighthouse.py`](experiments/tools/cl_vs_lighthouse.py) | `probe-cl` per-slot arrival vs Lighthouse log arrival |
| [`hedge_ab.py`](experiments/tools/hedge_ab.py) | adaptive vs fixed SNAP hedging on one node, interleaved per read (needs `--debug-rpc`) |
| [`latency_bench.py`](experiments/tools/latency_bench.py) | warm-path latency and concurrent throughput of an endpoint |
| [`learned_ab.py`](experiments/tools/learned_ab.py) | A/B of the same random Uniswap V3 quotes against two skim instances |
| [`mem_ab.py`](experiments/tools/mem_ab.py) | memory A/B: several builds side by side under the same load |
| [`p0_analyze.py`](experiments/tools/p0_analyze.py) | summarizes a `skim follow` JSONL: misses, rounds per call, tails |
| [`replay_check.py`](experiments/tools/replay_check.py) | `debug_traceTransaction` / `debug_traceBlockByNumber` vs a reference node |
| [`rpc_diff.py`](experiments/tools/rpc_diff.py) | differential JSON-RPC harness pinned to one block hash, with cold/warm latency |
| [`rpc_gaps_check.py`](experiments/tools/rpc_gaps_check.py) | block data, fees, access lists, proofs, tracers and `eth_simulateV1` vs a reference node |
| [`sim_trace_check.py`](experiments/tools/sim_trace_check.py) | state overrides, `eth_simulateV1` and `debug_traceCall` vs a reference node |
