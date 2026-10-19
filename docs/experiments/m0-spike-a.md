# M0 Spike A: skim's own CL gossip listener (`skim probe-cl`)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Runs on 2026-10-15 (KST) from the same home connection as round 2, with the local Lighthouse v8.3.0-rc.0 + Nethermind node running alongside (same public IP). Sepolia is on Glamsterdam (Gloas fork digest `0x669e6c11`).

Raw data:
- `data/probe-cl-run1.*`: 16 min, 80 slots
- `data/probe-cl-run2.*`: 12 min, 59 slots

Each run has a `.jsonl` with one line per block/envelope/session/status, a `.out` with the final summary, and a `.err` with the tracing log. The comparison script is `docs/experiments/tools/cl_vs_lighthouse.py`.

## What was built
**`crates/skim-cl`** is a small stack of our own. It does not use `lighthouse_network`.

| Piece | How |
|---|---|
| Discovery | discv5 0.12 + enr 0.14. Uses the Sepolia CL bootnodes from `eth-clients/sepolia`. Keeps ENRs whose `eth2[0..4]` equals the fork digest and that have `tcp`. Dials from query results and from `Discovered` events, with a 120 s redial backoff. Our ENR carries `eth2` (digest, next version `0x90000076`, far-future epoch), `attnets`, `syncnets`, `cgc=4`, `nfd`, plus `tcp`/`udp`/`quic` ports. It has no IP, so peers can't reach us inbound. |
| Transport | libp2p 0.57 (crates.io): TCP + noise + yamux. QUIC listens on port+1 but is not dialed. Identify sends `eth2/1.0.0`. |
| Gossipsub | Lighthouse's mesh and history parameters, anonymous validation mode. A snappy-block `DataTransform` decompresses each message so the message-id function sees SSZ. The message id is `sha256(0x01000000 ++ u64le(len(topic)) ++ topic ++ ssz)[:20]`, or the invalid-snappy domain over raw bytes. Messages that parse are reported `Accept` and forwarded; others are reported `Ignore`. |
| Req/resp | `ssz_snappy` framing over libp2p request-response. Each side half-closes and reads to EOF. Handles Status v2/v1, MetaData v3/v2 (seq 1, our attnets, cgc 4), Ping, and Goodbye (logs the reason). Block, blob, column and envelope range/root requests get zero chunks, which is a valid empty answer, so peers never see an unsupported protocol. |
| Status policy | See below. |
| Parsing | `ssz.rs` is a hand-written offset parser for `SignedExecutionPayloadEnvelope` and `SignedBeaconBlock`. From the beacon block it reads the slot, proposer, parent root and `signed_execution_payload_bid`. |
| Verification | `verify.rs` rebuilds the Amsterdam EL header and compares `keccak(rlp(header))` with `payload.block_hash`. It then decodes the BAL with `skim-bal`. |

**Status policy.** skim sends a status as the dialer and answers inbound status requests with its best-known status:
- **Finalized checkpoint:** the highest one any peer reported.
- **Head:** the newest of (a) any peer's head and (b) `(payload.slot_number, envelope.beacon_block_root)` from the latest gossiped envelope. So skim never has to compute a beacon block root.
- **`earliest_available_slot`:** set to the head, because skim serves no history.
- **Bootstrap:** before anything is learned, skim answers an inbound status by echoing the requester's own status. An outbound status uses a zero status (epoch 0, zero roots). Lighthouse classes that peer as "Behind" but relevant.

Lighthouse sorts prune candidates by sync state, so a fresh head matters for retention.

**`skim probe-cl`** (crates/skim-cli/src/probe_cl.rs) has defaults `--port 9110`, `--target-peers 50` and `--out data/probe-cl.jsonl`. `--capture-dir` saves raw SSZ, and `--peer` adds peers that are always dialed (diagnostics only; not used in the measured runs). It prints a summary every 30 s.

## SSZ: progressive types serialize like plain ones
Gloas (EIP-7688) changes `ExecutionPayload`, `ExecutionPayloadEnvelope`, `ExecutionRequests` and `BeaconBlockBody` into `ProgressiveContainer` (EIP-7495), and most lists into `ProgressiveList` (EIP-7916). Both EIPs change only merkleization, not serialization:
- A progressive container serializes exactly like a `Container` with the same fields in the same order.
- A progressive list serializes like a `List`.

Lighthouse's Gloas types use the ordinary `ssz_derive` encoding. Real traffic confirms it: every one of 139 envelopes rebuilt to the exact block hash.

Layout notes:
- **`ExecutionPayload`:** fixed part of 540 bytes (19 fields; 5 variable: `extra_data`, `transactions`, `withdrawals`, `block_access_list`, plus `slot_number` at the end).
- **`block_access_list`:** a `ProgressiveList[Byte]` holding the RLP BAL verbatim.
- **No slot in the envelope.** The slot comes from `payload.slot_number` (EIP-7843). Every payload had `timestamp == genesis + 12·slot_number`.
- **`ExecutionRequests` has 5 fields on Sepolia** (EIP-8282 adds builder deposits and builder exits). The local Lighthouse checkout of Oct 1 still has 3. The parser reads the field count from the first offset, so it accepts both.
- **`requests_hash`:** `sha256(‖ sha256(type ++ ssz(list)))` over the non-empty lists, with types 0–4.

## BAL verification: works (139/139)
skim rebuilds the header from:
- the payload fields
- `ommers_hash = EMPTY_OMMER_ROOT_HASH`
- `transactions_root`: ordered trie over the raw EIP-2718 bytes
- `withdrawals_root`
- `parent_beacon_block_root` from the envelope
- `requests_hash` from `execution_requests`
- `block_access_list_hash = keccak(raw BAL)`
- `slot_number`

Results across both runs:
- **Header hash:** `keccak(rlp(header)) == payload.block_hash` for **139/139** envelopes (0 mismatches).
- **BAL decode:** `skim_bal::decode` succeeded for 139/139. BALs had 177–1,316 accounts and up to 103 KB of RLP. Envelopes were up to 226 KB.
- **Bid link:** `payload.block_hash == bid.block_hash` in the beacon block of the same slot for 138/138 slots where skim had seen that block (one envelope arrived before its block).
- **Cost:** header rebuild plus BAL decode takes P50 0.29 ms, P95 0.46 ms in a release build. It runs inline and does not need offloading.
- **Independent check:** for the fixture (slot 11356920, block 11916226), the local Nethermind's `eth_getBlockByNumber` gives the same hash, `stateRoot`, `blockAccessListHash`, `slotNumber` and tx count.

This is the full §3.2 check without BLS: the BAL is bound to `block_hash`, and `block_hash` is bound to the proposer-signed block's bid. BLS signature checks (builder and proposer) are still TODO.

## Arrival lag (receive time − slot start)
The comparison uses the Lighthouse log lines `New envelope received` and `New block received` for the same slots (KST → UTC).

Lighthouse logs these lines after its gossip verification. Its numbers are therefore "verified at Lighthouse", while skim's are "decompressed and parsed at skim". Lighthouse's real arrival time is somewhat earlier than its log line.

| | skim P50 | skim P95 | Lighthouse P50 | Lighthouse P95 | Δ skim−LH P50 (per slot) | skim earlier |
|---|---|---|---|---|---|---|
| Envelope, run 1 (77 matching slots) | 732 ms | 1,648 ms | 655 ms | 2,027 ms | +39 ms | 39% |
| Envelope, run 2 (59) | 896 ms | 2,919 ms | 721 ms | 2,748 ms | +193 ms | 19% |
| **Envelope, both (136)** | **823 ms** | **2,665 ms** | **675 ms** | **2,594 ms** | **+118 ms** | 30% |
| Beacon block, both (137) | 284 ms | 1,790 ms | 324 ms | 1,783 ms | −10 ms | 71% |
| Block → envelope at skim | 366 ms (run 1), 524 ms (run 2) | 848 / 1,257 ms | | | | |

- **Beacon blocks:** skim matches Lighthouse within a few ms at P50 and is earlier in 71% of slots.
- **Envelopes:** skim is about 0.1–0.2 s behind Lighthouse's verified-log time at P50. Per-slot P95 is +0.5 s, and the worst slot was +2.2 s.
- **Why envelopes lag:** skim's mesh on `execution_payload` is small and churns (1–8 peers, often 3). Some envelopes therefore come through IHAVE/IWANT rather than mesh push. Blocks are small and arrive from many peers at once. Envelopes are 100–230 KB and travel fewer paths.
- **No missed slots:** skim saw every slot Lighthouse logged within each run's window.

The round-2 headline still holds with skim's own stack: the BAL is in hand at about 0.8 s P50 after slot start. That is roughly 0.85 s ahead of the EL path (about 1.67 s P50).

## Peer retention: a churny but working core
| | Run 1 (16 min) | Run 2 (12 min) |
|---|---|---|
| Time to first peer | 1.7 s | 1.9 s |
| Connected at end (target 50) | 51 | 50 |
| Connected-at-end session age | n/a | P50 555 s; 37 of 50 over 300 s |
| Sessions opened / closed | 410 / 359 | 293 / 243 |
| Closed sessions with a completed Status, duration | P50 ≈ 19 s, P95 ≈ 240 s, max 802 s (both runs) | |
| Goodbyes received | 59 × 129 too_many_peers, 1 × 3 | 42 × 129 too_many_peers, 1 × 250 bad_score, 1 × 3 |
| Irrelevant / our goodbyes | 0 / 0 | 0 / 0 |

Clients seen: almost all Lighthouse v8.3.0-rc.0, plus some Prysm (all of which said `too_many_peers`), and single sessions from tysm and erigon. Sepolia Gloas peers are mostly full infrastructure nodes.

How sessions end:
- **Early drops.** Around 37–40% of sessions end within 0.5 s without a Status: "protocol negotiation failed" or "broken pipe", presumably from peers at their connection limit.
- **Pruning.** Most Lighthouse sessions that completed a Status end at that peer's next 30 s peer-manager heartbeat (`prune_excess_peers`).
- **The stable core.** Peers that are under their peer target keep skim. By minute 10, 37 of 50 peers had been connected for over 5 min.

What mattered for retention (found by connecting to the local Lighthouse with `--peer` and reading `/lighthouse/peers`, read-only):
1. **Gossipsub subscription filter.** libp2p's default `MaxCountSubscriptionFilter` drops any RPC carrying more than 100 subscriptions. Eth2 peers announce hundreds of topics at once, so their SUBSCRIBE and GRAFT messages were silently ignored. The fix is to track only skim's topics, with a 4,096 per-request limit.
2. **A long-lived subnet.** Lighthouse prunes peers in this order:
   1. negative score
   2. `!has_long_lived_subnet()`, which needs non-zero `attnets` in MetaData/ENR *and* a subnet subscription
   3. densest custody-column subnet, sorted by `(custody_subnet_count, is_synced)`

   skim now advertises and joins 2 attestation subnets, chosen as a simple function of the node id rather than the spec's shuffled `compute_subscribed_subnets`. It forwards `SingleAttestation`s after a size-only check (240 bytes). Run 2 relayed 234.
3. **A fresh head in Status.** "Behind" peers sort first for pruning at step 3, so the head now follows gossiped envelopes.

Remaining risks:
- We advertise `cgc=4` but don't subscribe to column subnets or serve columns. By-root/by-range requests get empty responses (only 1–2 seen per run).
- Attestations are relayed unvalidated.
- With no IP in the ENR there are no inbound peers. That costs little, since public peers are full anyway.
- One peer gave us `bad_score` (250) in run 2. The cause is unknown; possibly the unvalidated relays, or the empty responses.

## What works / what doesn't
| | Status |
|---|---|
| Works | Discovery and connection to Sepolia Gloas peers in under 2 s. Status/MetaData/Ping/Goodbye. Envelopes and blocks received on 100% of slots. Correct message ids (unit-tested against Python `hashlib` on captured messages). Offset parse of envelope, block and bid. Full EL-header rebuild ties the BAL to `block_hash` and `block_hash` to the bid. 50 peers held with a stable core. |
| Doesn't yet | BLS verification. Beacon block root computation (would need progressive merkleization). Spec-exact attestation-subnet selection and attestation validation. Data-column custody (we advertise `cgc=4` without serving it). Inbound reachability (no ENR IP / NAT). QUIC dialing. Matching Lighthouse's envelope P50 (+0.1–0.2 s): mesh sizes on `execution_payload` stay small. |
| Not needed | `lighthouse_network`. The crates.io libp2p 0.57 stack worked without the sigp fork or `[patch]`es. |

## Next
- Hold a larger, more stable `execution_payload` mesh. Options: explicit or pinned infra peers, a higher `mesh_n` for that topic, and/or real column custody so peers value us.
- Then re-measure the envelope P50 gap.
- Feed `verify_payload` output straight into the state follower (`skim-bal::BlockWrites`), with the "candidate until selected" rule from §3.2.

## Tests
`cargo test -p skim-cl` runs these tests:
- `rpc` unit tests: Status/MetaData v1/v2/v3 round trips, varint, `ssz_snappy` framing.
- `tests/envelope.rs`, on a captured real envelope and block (slot 11356920):
  - field values
  - header rebuild equals block hash
  - BAL decode with commitment check
  - a tampered BAL breaks the hash
  - bid linkage
  - truncation rejected
  - message ids for valid and invalid snappy
