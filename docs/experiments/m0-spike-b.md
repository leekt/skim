# M0 Spike B: EL P2P feasibility (first results)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs: 2026-10-14, about 30 minutes in total. Run from one home connection (Korea). Command: `skim probe-el` (reth v2.7.0 networking, eth/71 + snap/2 client, DNS and discv4 discovery). Raw samples are in `data/probe-el-*.jsonl`.

**Self-check:** the EIP-2935 history contract stores the parent hash of block n in slot `(n-1) % 8191`. Every value read from a BAL or from SNAP was compared against `header(n).parent_hash`, so no reference node was needed.

## Results

Combined results from run 1 (10 min) and run 3 (15 min):

| Metric | Run 1 | Run 3 |
|---|---|---|
| Blocks followed | 49, no gaps or reorgs | 71, no gaps or reorgs |
| EL header-poll lag from slot start, P50 | 1.03 s | 1.31 s |
| EL header-poll lag, P95 | 2.0 s | 47 s ¹ |
| eth/71 BAL, P50 / P95 | 376 / 917 ms | 363 / 1079 ms |
| BAL commitment + self-check | **63/63** | **119/119** |
| BAL `unavailable` (0x80) | 0 | 0 |
| snap/2 account lookup, P50 / P95 | 768 / 964 ms | 608 / 1455 ms |
| snap/2 storage lookup, P50 / P95 | **291 / 526 ms** | **291 / 579 ms** |
| SNAP value self-check | **12/12** | **28/28** |
| Root-servable lag (header seen → first successful answer) ² | P50 778 ms | P50 609 ms |
| Peer that never served a new root | 1 (Nethermind) | 0 |

¹ The P95 header lag comes from periods with zero connected peers, when polling stalled. It measures peer retention, not protocol latency.
² In practice this equals the first account-request latency. Serving peers answered on the first try, so they already had the root when we saw the header.

## Peer census (run 3, 15 min)

| Client | Advertised caps | Notes |
|---|---|---|
| Nethermind v2.1.0 | `eth/68–72, snap/1, snap/2` | **The only snap/2 servers.** Frequently `TooManyPeers` |
| Geth v1.17.7 | `eth/69–72, snap/1` | **snap/1 only**. Most of the real nodes still connected at the end |
| reth v2.7.0 | `eth/66–71` | no snap (reth's default is snap off); `TooManyPeers` |
| enrscout | crawler | 31 sessions, closed immediately |

## Findings
1. **The protocol path works.** BALs by hash, verified against the header commitment, plus SNAP point lookups at a fresh root, reproduce correct state with zero mismatches (147 BALs, 40 slot reads). This is the core feasibility assumption.
2. **A storage point lookup costs about 290 ms P50 / 550 ms P95** from this location, over whichever peer was connected. Trusted, low-RTT peers (Spike B2) should improve this. `peer_rtt_ms` isn't measured yet.
3. **Account lookups are about 2× slower than storage lookups.** It's unclear whether this comes from message type or from being the first request at a fresh root. Next step: repeat the account request at the same root.
4. **Peer availability is the real constraint:**
   - Public nodes are full (`TooManyPeers`), and it takes minutes to collect even 5–7 real peers.
   - snap/2 is served only by Nethermind. reth's snap client is snap/2-only, so the Geth majority, which offers snap/1, is unusable today.
5. **EL header polling sees heads about 1.0–1.3 s after slot start (P50).** This is the baseline that CL payload gossip (Spike A) has to beat.

## Next steps
- **snap/1 client support.** `GetAccountRange`, `GetStorageRanges` and `GetByteCodes` (0x00–0x05) are the same in snap/1 and snap/2, except for the slim-account encoding. A snap/1 RLPx subprotocol client, alongside reth's snap/2, would make Geth peers usable as state sources. This is the biggest lever on availability.
- **Spike B2: trusted pool.** Pick 3–5 peers from the per-peer scorecard (`per_peer` in the probe summary, now with enodes), keep them connected with `--trusted`, and measure storage P50/P95 and retention over hours.
- **Separate RTT from serving time.** Measure `peer_rtt_ms`, and repeat account requests at the same root (finding 3).
- **Spike C.** Compare against Infura `eth_call` at the same block. This needs an Infura Sepolia URL in an environment variable, used only by the harness.
- **Spike A: CL payload gossip.** Use Lighthouse v8.3.0-rc.0 networking (progressive SSZ). Baseline to beat: 1.0–1.3 s EL header lag, plus about 360 ms BAL fetch.
