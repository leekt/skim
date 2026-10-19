# Future considerations

Directions considered and deliberately not built yet. Each says what it would add, what it would cost, and why it waits. None of this is available in skim today.

## State-root verification without the full state
skim checks every value it holds through the BAL commitment (applied writes) or a Merkle proof (fetched values). It doesn't hold the full state, so it can't recompute a block's `stateRoot` after applying a BAL, which a full node can ([trust and correctness](trust-and-correctness.md)).

Geth's partial-state proposal ([go-ethereum PR 33764](https://github.com/ethereum/go-ethereum/pull/33764), open as of 2026-10) keeps **every account** but only selected contracts' storage and code, so it can recompute the account-trie root from BALs. Its author puts that at ~59 GiB with no contracts tracked, against ~640 GiB for a full node.

The open question for skim: **can the state root be checked from a minimal witness, without storing all accounts?** For example, keeping the trie paths of the held keys and updating them per block from BAL changes plus the sibling nodes those changes need. That would keep skim's footprint app-sized while adding a per-block root check. It may need witnesses that BALs don't carry, and it conflicts with skim's small-working-set goal if it degrades into holding the account trie. Research, not a roadmap item.

## A validation-state mode (VOPS, AA-VOPS)
[Validity-Only Partial Statelessness](https://ethresear.ch/t/a-pragmatic-path-towards-validity-only-partial-statelessness-vops/22236) (VOPS; ethresear.ch, 2025-04) is an architecture proposal, not a client: nodes keep, per EOA, only address, nonce, balance and a code flag (~40 bytes; the post estimates ~8.4 GiB against ~233 GiB of full state, "a 25x storage reduction"), enough to check that a pending transaction is valid. The goal is censorship resistance under FOCIL: a valid-only mempool that every node can enforce. The post names EIP-7928 BALs as the way to publish the per-block account changes. It's one of the motivations behind Geth's partial-state PR.

Native account abstraction breaks the "nonce and balance are enough" assumption. **AA-VOPS**, in the same post, has each node keep only the accounts it cares about (the account leaf, the storage slots validation needs, and a Merkle path witness), refreshed from per-block state diffs. That is close to skim's model: subscribed keys kept current from BALs, plus witnesses.

A possible skim direction: alongside the **application state** it keeps for `eth_call`, simulation and indexing, a **validation state** for pending-transaction checks (nonces, balances, the keys and policies that authorize a smart account). Caveats:
- A mempool-validation working set differs from an `eth_call` working set.
- Using it for a censorship-resistant mempool needs the validity rules and state availability the protocol requires. skim's selective cache alone doesn't provide those guarantees.

## Embedding: skim as a library
Today skim is a process serving JSON-RPC. As a library, an application (an SDK, a desktop or mobile wallet, a Node.js service through a native addon) could link P2P networking, the partial-state store, local EVM and consensus verification directly, with no RPC hop. The crates are already split along these lines (`skim-el`, `skim-cl`, `skim-state`, `skim-sim`, `skim-chain`); the work is a stable embedding API, lifecycle control (start, stop, persistence), and builds for the target platforms.

## Peer privacy
skim uses no RPC provider, but the SNAP peers it asks see which accounts and storage slots it reads. Being peer-to-peer doesn't make reads private. An opt-in mode could route state requests over Tor or spread them across peers, at a latency cost that makes it a choice, not a default.

## RPC conformance against Geth
skim refuses unsupported JSON-RPC parameters rather than ignoring them, and its answers are compared with a reference node in the soaks and on the devnet (`devnet/check.py`, `examples/devnet-edgecases.mjs`). A conformance suite would systematize that: for each method, parameter combination and error case, compare skim's result or error with Geth's at the same block hash.
