# P0: verified SNAP lookups (nothing unverified becomes state)

> Historical experiment record. Measurements, commands and implementation details below describe the builds tested, not necessarily the current release. For current behavior, see the [configuration](../guide/configuration.md), [RPC reference](../guide/json-rpc.md) and [architecture](../../PROJECT.md).

Commit `5d65e4a`, follow-ups `04af043`. This replaces the earlier rule, which re-asked the same peer for the account and took zero if its storage root was empty. That rule wasn't a proof: it couldn't show that a given slot is absent from a **non-empty** storage trie.

## The rule now
| SNAP answer | Treated as |
|---|---|
| The peer can't serve the root (empty, no proof) | **Unavailable**: retry another peer or later; never a value |
| Account: proof against the state root shows presence | that account (`TrieAccount` from the proven leaf) |
| Account: proof shows absence | absent account: all of its slots are zero |
| Account's verified storage root is the empty trie | every slot is zero |
| Slot: proof against the verified storage root shows presence | that value |
| Slot: proof shows absence in a non-empty trie | zero (proven) |
| No proof, but the response is a whole trie that rebuilds to the storage root | read from that complete set |
| No proof, a proof that doesn't settle the key, a tampered node, or a response that contradicts its own proof | **Unavailable**, counted by reason |

Implementation:
- `skim-snap::proof::lookup` walks the hashed key from the expected root through the peer's proof nodes. Nodes are indexed by hash, so SNAP's combined boundary proofs work as they are.
- `skim-snap::el::ElTransport` verifies accounts against the state root, and slots against the account's *verified* storage root. Account and slot are fetched in parallel. The account fetch is shared per `(root, address, peer)`. It's keyed per peer so a slow peer can't block lookups routed elsewhere; the first version shared it across peers and caused 30 s stalls.

## Invariants and tests
- **The MVCC store never treats Unknown as zero** (`skim-state`: `follows_writes_and_unknown_is_not_zero`).
- **Proofs** (`skim-snap::proof` tests):
  - inclusion and exclusion
  - a missing proof or a proof for another root is an error, not absence
  - a truncated proof is never read as absence
  - a tampered node breaks the hash chain
  - the empty root, and the whole-trie rebuild check
- **Scheduler fault injection** (`skim-snap` tests):
  - a peer that can't verify is never turned into zero: the value comes from another peer, or the lookup fails (`empty_answer_from_unserving_peer_is_never_zero`)
  - **delayed root availability:** the only peer is unavailable for 3 requests, then serves; the result is the value, never zero (`root_served_late_yields_the_value_never_zero`)

## Live results
- **RPC diff vs Infura with verification on:** 12/12 match. Public Nethermind (snap/2) and Geth (snap/1) peers return valid boundary proofs for point lookups. Rejections: **0** (no-proof 0, bad-proof 0, contradiction 0).
- **Randomized follower (`data/p0-V.*`, 60 blocks, 480 unseen V3 quote inputs):**
  - **866/866 outputs match Infura**; the other 34 couldn't be compared because Infura returned no answer
  - 0 post-run misses
  - 11 predicted fetch waves, P50 396 ms

Verification adds one account lookup per contract and root, in parallel with the slot lookups, so it adds no RTT to the wave. It also turns skim's state from "trusted peers" into "verified against the block's state root". The state root itself is still trusted from the gossiped, header-hash-verified payload (§8: consensus is not verified yet).
