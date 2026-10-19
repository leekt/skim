# M1: BAL state follower, end to end (`skim follow`)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs on 2026-10-15 (KST), same home connection (Korea). Each run followed 100 consecutive Sepolia blocks after bootstrap. Raw data is in `data/follow-1.*` and `data/follow-2.*`; numbers below are from run 2.

## Pipeline
1. **Receive:** CL gossip `execution_payload` envelope.
2. **Verify:** rebuild the EL header from the payload, with `block_access_list_hash = keccak(BAL)`, and check that its hash equals `block_hash`.
3. **Apply:** normalize the BAL and apply it to the per-key MVCC store (`skim-state`). The state is now ready.
4. **Simulate:** run the 7 watched `eth_call`s locally on the new snapshot (revm, Amsterdam rules), with no network.

**Bootstrap** runs once, after SNAP peers exist. Each call is resolved over SNAP, and its access set is committed as MVCC bases. After that, BAL writes keep those keys current. A key the calls touch for the first time (a "miss") is fetched over SNAP at that block's root.

**Gaps** in gossip are filled from EL peers (headers + eth/71 BALs). None occurred.

## Results (run 2, 100 blocks; times are ms after slot start)

| Stage | P50 | P95 |
|---|---|---|
| Envelope received | 880 | 2,946 |
| BAL verified and applied (state ready) | 880 | 2,947 |
| All 7 calls simulated | **887** | 3,036 |
| Local simulation time, all 7 calls | 3.8 ms | 3.9 ms |
| BAL apply (verify excluded) | 0.16 ms | 0.21 ms |

- **Chain following:** 100/100 blocks came from gossip, with 0 EL gap fills and 0 reorgs.
- **Correctness:** **700/700 outputs match Infura** at the same block hash.
- **Network use after bootstrap:** 0 SNAP requests on 99 blocks. On 1 block a new key appeared (likely a tick crossing). That one miss took **7.4 s** to resolve over SNAP.

## Versus Infura, measured fairly
An independent poller sends Infura `eth_call` at block **number** n, starting at slot n + 300 ms and repeating every 50 ms, on a keep-alive client. Infura **holds** a request for a block it doesn't have yet and answers once the block arrives. So the poller's first answer time is when an Infura user would have the result.

| | P50 | P95 |
|---|---|---|
| Infura user gets the first answer for block n | 756 | 3,448 |
| skim finishes all 7 calls for block n | 887 | 3,036 |
| Per-block difference (Infura − skim) | **0** (P10 −335, P90 +259) | |
| skim finishes first | **50 of 99 blocks** | |
| Infura warm keep-alive `eth_call` | 236 | |

**Conclusion: for the first answer on a new block, skim and Infura are tied.** Infura's nodes appear well connected to block producers. skim's own gossip mesh is small: its envelope P50 is 880 ms here, against 675 ms for the local Lighthouse in Spike A.

**Where skim clearly wins is everything after the first answer.** All 7 calls cost 3.8 ms locally, with no network. An Infura user pays about 236 ms per call (warm), or more for sequential calls. A solver evaluating many candidate routes per block gets them in microseconds once the block's state is ready.

### Run 1 (for the record)
Run 1 reported "skim leads Infura by P50 226 ms". That number was a measurement artifact. Infura was polled only *after* skim finished, so the lead equalled Infura's RTT. Run 2 replaced that method.

## Implications
1. **State readiness is bounded by gossip mesh quality, not by skim's work.** Verify plus apply plus 7 calls takes under 5 ms. To get ahead of Infura, skim needs the envelope sooner:
   - more and better-connected `execution_payload` mesh peers
   - direct peering with well-connected nodes
   - inbound reachability (our ENR has no IP)
2. **The miss path is the tail risk.** One new key cost 7.4 s. Options:
   - prefetch keys adjacent to the learned set (e.g. neighbouring tick bitmap words)
   - use BAL `storage_reads` of the watched contracts
   - take misses off the critical path by returning with a "stale for key X" signal
3. **Revised product claim:** "same first-answer latency as a hosted RPC, then unlimited local calls per block at microsecond cost, with no full node." It is *not* "earlier than Infura" yet.
