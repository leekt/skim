# Future considerations

Directions considered and deliberately not built yet. Each says what it would add, what it would cost, and why it waits. None of this is available in skim today.

## State-root verification without the full state
skim checks every value it holds through the BAL commitment (applied writes) or a Merkle proof (fetched values). It doesn't hold the full state, so it can't recompute a block's `stateRoot` after applying a BAL, which a full node can ([trust and correctness](trust-and-correctness.md)).

Geth's partial-state proposal ([go-ethereum PR 33764](https://github.com/ethereum/go-ethereum/pull/33764), open as of 2026-10) keeps **every account** but only selected contracts' storage and code, so it can recompute the account-trie root from BALs. Its author puts that at ~59 GiB with no contracts tracked, against ~640 GiB for a full node.

The open question for skim: **can the state root be checked from a minimal witness, without storing all accounts?** For example, keeping the trie paths of the held keys and updating them per block from BAL changes plus the sibling nodes those changes need. That would keep skim's footprint app-sized while adding a per-block root check. It may need witnesses that BALs don't carry, and it conflicts with skim's small-working-set goal if it degrades into holding the account trie. Research, not a roadmap item.

## Participating in the network, not just consuming it
Long-term goal: **a lightweight Ethereum client that participates in the network, not just consumes it.** Three roles on one partial-state engine. Users run the light client by default and opt into the others with the resources they want to give.

| Role | Weight | Fit with skim |
|---|---|---|
| Verified lightweight client | core | very high |
| Light mempool participant | core extension | very high |
| Recursive proof aggregator | optional, by hardware | medium, until its cost is known |

**Three kinds of verification**, kept apart, because improving one doesn't improve the others:

| | Today | With [EIP-8288](https://eips.ethereum.org/EIPS/eip-8288), if skim verifies the block's STARK |
|---|---|---|
| **Consensus:** which block is canonical | sync-committee light client, not full fork choice; `latest` is optimistic | unchanged |
| **Signatures and proofs:** are the block's authorizations valid | taken on trust from consensus | checked: one recursive STARK per block |
| **Execution:** are the block's results and state root correct | not checked: BAL post-values trusted through the header commitment; no state-root recompute | unchanged |

Verifying the block STARK would not make skim a validating node, and nothing should claim it does.

### 1. The verified lightweight client
What skim is today (subscribed state kept current from BALs, proofs for fetched values, local execution, a sync-committee light client), plus, once EIP-8288 lands, verifying the block-level recursive STARK: every signature and proof a block depends on, checked at roughly one proof verification per block. Prerequisites:
- **EIP-8141 frame transactions** in the networking and RPC stack: the sender stops being `ecrecover`, sending means a mode-0 wrapper (transactions plus their proofs), and gas estimates add `recursive_stark_gas`.
- **Headers without the proof:** EIP-8288 adds `recursive_stark` to the header, which "does increase the block header size greatly". skim keeps 8,192 headers in memory and every post-fork header on disk with `--history local`, so it would verify the proof and keep only the hash links and `block_deps_hash`.

### 2. A light mempool participant
Under EIP-8288, signatures are proven by STARKs, but a mempool node still has to know whether a transaction is **includable**, which is a question about state: nonce, balance, and under native account abstraction, whatever the account's validation reads. A full node answers it with all of the state. skim can keep just the validation state, and its existing machinery for `eth_call` subscriptions is that mechanism already: run the call, learn its access set from execution, subscribe those keys, re-check when a BAL touches one.

```
incoming transaction
  -> run its validation (frame) locally; the access set is its dependencies
  -> fetch the missing state (SNAP, with proofs)
  -> keep those keys current from BALs; re-validate when one changes
  -> validate / relay / aggregate
  -> release the keys on inclusion or expiry
```

This is the [VOPS](https://ethresear.ch/t/a-pragmatic-path-towards-validity-only-partial-statelessness-vops/22236) idea (nodes keep only what pending-transaction validity needs; ~40 bytes per EOA, "a 25x storage reduction", in the post's estimate) and its native-AA variant, AA-VOPS (each node keeps only the accounts and slots validation needs, with witnesses, refreshed per block). Caveats:
- **Bounded validation is the protocol's job.** If validation can read arbitrary state, a mempool node can be made to fetch and track unbounded state per transaction (a DoS vector). skim has to follow the protocol's validation rules, as ERC-4337's validation rules did for user operations, not invent its own.
- **A selective cache isn't a mempool guarantee.** A censorship-resistant mempool (FOCIL) needs the validity rules and state availability the protocol requires.
- **A first measurement is possible today:** the validation working set of Sepolia's current mempool (senders' nonces and balances, delegated accounts' code) and how fast it churns. That's `skim probe-mempool`.

### 3. A recursive proof aggregator
In EIP-8288, mempool nodes rebuild a recursive STARK over their valid transactions every `AGGREGATION_INTERVAL` (1,000 ms), and builders do the same over what they include. Proving capacity scales with how many nodes aggregate, which makes this role matter long-term. But its cost depends on an implementation and parameters that are still TBD, so no plan should assume a typical skim produces a recursive STARK every second. Partial participation (smaller aggregates, the EIP's priority order) may be what fits cheap hardware. Measure first.

### On "more nodes, more security"
Conditionally true. More light clients mean wider access and more independent checking; more relays diversify propagation and can improve censorship resistance; more aggregators spread and scale proving. None of it is automatically more consensus security or more throughput: topology, aggregation scheduling, duplicate work and censorship resistance decide that.

## Embedding: skim as a library
Today skim is a process serving JSON-RPC. As a library, an application (an SDK, a desktop or mobile wallet, a Node.js service through a native addon) could link P2P networking, the partial-state store, local EVM and consensus verification directly, with no RPC hop. The crates are already split along these lines (`skim-el`, `skim-cl`, `skim-state`, `skim-sim`, `skim-chain`); the work is a stable embedding API, lifecycle control (start, stop, persistence), and builds for the target platforms.

## Peer privacy
skim uses no RPC provider, but the SNAP peers it asks see which accounts and storage slots it reads. Being peer-to-peer doesn't make reads private. An opt-in mode could route state requests over Tor or spread them across peers, at a latency cost that makes it a choice, not a default.

## RPC conformance against Geth
skim refuses unsupported JSON-RPC parameters rather than ignoring them, and its answers are compared with a reference node in the soaks and on the devnet (`devnet/check.py`, `examples/devnet-edgecases.mjs`). A conformance suite would systematize that: for each method, parameter combination and error case, compare skim's result or error with Geth's at the same block hash.
