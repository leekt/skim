# Trust and correctness

skim is built so that it can be **slow or unavailable, but not wrong** about the state it serves, within the trust model below.

## What skim verifies
| Data | Check |
|---|---|
| Block access list | The EL header is rebuilt from the gossiped payload (including `keccak(BAL)`) and must hash to the payload's `block_hash`. |
| Gap-filled blocks | Headers must link by parent hash. Each BAL must match its header's `block_access_list_hash`. |
| State fetched over SNAP | Every account is Merkle-proven against the block's `stateRoot`, and every slot against that account's `storageRoot`. Zero is accepted only with a proof of absence. |
| Contract code | `keccak(code) == codeHash` |
| Receipts | Rebuild `receiptsRoot` |
| Bodies (older transactions) | Rebuild `transactionsRoot` |
| Headers before skim started | Hash-linked to a known header |
| Light-client bootstrap and updates | Trusted checkpoint root; sync-committee Merkle branches and BLS signatures, with committee size and period from the configured preset |

## Two different guarantees
| Guarantee | Status |
|---|---|
| **A value matches the block it claims to be from** (state root, receipts root, block hash) | Checked, for every value skim serves |
| **Consensus authentication of the selected block** | `safe` and `finalized` resolve only verified execution hashes on skim's selected chain. `--require-verified` also requires the signed block for `latest`. The light client must have bootstrapped and received usable updates. |

A Merkle proof shows that a value belongs to a root. It doesn't show that the root belongs to the right chain. Default `latest` is the newest state-ready gossiped block, so it can be served before consensus authentication. Use `safe`, `finalized`, or `--require-verified` when that distinction matters; these return `-32002` until the relevant verified block is available on the selected chain.

## Bootstrap and availability

The checkpoint source is, in order: `--checkpoint`, the last verified finalized root saved in the data dir, or `--checkpoint-url`. Sepolia has a default checkpoint-sync URL; a custom network needs an explicit or saved root, or its own URL. The starting root is a trusted weak-subjectivity checkpoint. State and light-client updates are then fetched over P2P and checked; there is no external Ethereum JSON-RPC fallback for state reads.

The light client supports both `mainnet` and `minimal` consensus presets and catches up across committee periods. End-to-end bootstrap, updates and rotation have been verified on the [minimal-preset devnet](../../devnet/README.md). Earlier Sepolia runs encountered unavailable Gloas light-client data; this is a peer-availability observation, not a permanent network restriction. `skim_status.light_client` reports `period`, `verified_block`, `finalized_block`, accepted/rejected updates, checkpoint provenance and `last_error`.

## Three kinds of verification
| | What skim does | What that leaves |
|---|---|---|
| **Consensus:** which block is canonical | a sync-committee light client: `safe` and `finalized` are authenticated, `latest` is optimistic | not full fork choice or attestation validation |
| **Signatures and proofs:** are the block's transactions authorized | taken on trust from consensus | (EIP-8288 would make this checkable with one STARK per block: [future considerations](future-considerations.md#participating-in-the-network-not-just-consuming-it)) |
| **Execution:** are the results and state root correct | BAL post-values applied as committed by the header; fetched values checked by Merkle proof | no re-execution, no state-root recompute |

Improving one doesn't improve the others.

## What skim does not verify (yet)
- **Full validator consensus and fork choice.** skim uses a sync-committee light client rather than validating every proposer/attestation and running a full consensus client. Default `latest` remains optimistic; authenticated tags require verified updates and a matching selected-chain block.
- **Execution of the block itself.** skim takes the BAL's post-values as given, because they are committed by the block hash. A block that is valid by hash but whose BAL disagrees with its own execution would be invalid under consensus, and is out of scope here. For a consensus-authenticated block (`safe`, `finalized`) the post-values are therefore as trustworthy as the header; on optimistic `latest`, a gossiped block that consensus later rejects is the remaining risk.
- **The full state root.** skim holds only part of the state, so it can't recompute `stateRoot` after applying a BAL. A full node that catches up from BALs can, and does: Nethermind's BAL catch-up ([PR 14157](https://github.com/NethermindEth/nethermind/pull/14157)) recomputes and compares it for every reconstructed block, and applies BALs only to ancestors of the finalized block (checked by hash, not by number), executing the rest. skim's checks for the keys it holds are the BAL commitment and, for fetched values, Merkle proofs; `devnet/` compares those keys with an executing node at the same block hash across the same edge cases Nethermind tests ([devnet README](../../devnet/README.md#state-edge-cases)).
- **Networks without EIP-6780.** Taking post-values as the new state is complete only when SELFDESTRUCT can delete just accounts created in the same transaction (EIP-6780, Cancun). Every Glamsterdam network has it; `--network` refuses a `genesis.json` where Cancun isn't active by `amsterdamTime`.

## Rules that hold everywhere
1. **Unknown is never zero.** A value is used only if a BAL wrote it or a proof established it. The same holds for `BLOCKHASH`: an unknown hash is a miss, resolved from hash-linked ancestor headers or refused, never read as zero.
2. **Fetch at the parent, replay to the head.** At gossip time most peers haven't executed block N yet. skim reads at N−1's root and applies BAL(N) itself.
3. **Reorgs roll back to the common ancestor only** (in memory and in the archive), then replay.
4. **Errors instead of guesses.** If a read can't be proven, the RPC answers `-32002`.

## How it was tested
State and RPC behavior were compared on Sepolia against reference nodes at the same block hashes, and state edge cases on the devnet against Nethermind. Consensus verification also has unit tests for both presets and the devnet check above. See [`docs/experiments`](../experiments):

| Experiment | Result |
|---|---|
| 1-hour soak | 1,753 calls, 0 mismatches |
| 2-hour subscription soak | 0 disagreements |
| Randomized, never-seen V3 quotes | 2,700, all identical |
| Verified-state runs | 866 calls, 0 mismatches |
| Archive | 90/90 and 51/51 historical reads; backfill 102 identical, 3 refused, 0 wrong |
| Receipts and logs | 32/32 receipts and transactions; every `eth_getLogs` filter identical over 16 and 1,024 blocks |
| Devnet state edge cases (`examples/devnet-edgecases.mjs`: EIP-7702 delegation and clearing, SELFDESTRUCT, reverted writes, …) | all 11 steps match Nethermind at each block hash ([devnet](../../devnet/README.md#state-edge-cases)) |

A bug found by these checks (an empty SNAP answer treated as zero) is why rule 1 exists.
