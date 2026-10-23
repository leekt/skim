# Hegota (experimental)

Hegota is the fork after Glamsterdam: Heze on the consensus layer, Bogotá on the execution layer. Its spec isn't final. As of 2026-10, the meta EIP ([EIP-8081](https://eips.ethereum.org/EIPS/eip-8081), a draft) schedules two EIPs, FOCIL inclusion lists ([EIP-7805](https://eips.ethereum.org/EIPS/eip-7805)) and frame transactions ([EIP-8141](https://eips.ethereum.org/EIPS/eip-8141)), and considers twenty more.

skim implements Glamsterdam. On a network that schedules Hegota, it does one of two things:

| | What skim does at Hegota |
|---|---|
| **Default** | **Stops.** It applies no block from Hegota's activation on. Once the fork is reached, `latest` and `pending` answer `-32002` with reason `fork_unsupported` instead of the frozen state; explicit blocks before the fork and `safe`/`finalized` still answer. Warnings start at startup, and `skim status` fails once the fork is reached. If Hegota is already active, `skim run` refuses to start. |
| **`--experimental-hegota`** | **Follows it, under the assumptions below.** `skim status` warns after activation that answers rest on them. |

Any other scheduled fork skim doesn't know is always a stop, flag or not:
- a `FOO_FORK_EPOCH` in `config.yaml`, including devnet feature forks like `EIP8198_FORK_EPOCH`;
- a `fooTime` in `genesis.json`.

Each half activates on its own: Heze by epoch, Bogotá by block timestamp. A network may schedule only one of them; bogota-devnet-1 runs Bogotá on Gloas.

`skim_status.network.forks` shows the plan (`hegota: {at, support}`, `stops_at: {fork, at, reached}`).

## What experimental support does
- **Fork digest, ENR, light client.**
  - Heze's fork version (`HEZE_FORK_VERSION`) gets its own gossip digest. skim subscribes to it two epochs before the fork, like a blob-parameter fork.
  - The ENR advertises Heze as the next fork.
  - Sync-committee signatures verify with the version of the slot before the signature slot.
- **Block layout.** Beacon blocks at or after `HEZE_FORK_EPOCH` are read with the drafted Heze body: no `eth1_data` and no `deposits` (EIP-8015). The payload bid gains `inclusion_list_bits` (EIP-7805) after the fields skim reads. Each layout refuses the other's bytes instead of misreading them.
- **EL fork id.** Bogotá (`bogotaTime`) is in reth's fork list, so execution peers past the fork stay compatible.
- **Frame transactions (EIP-8141).** skim decodes type-`0x06` transactions and their receipts in block bodies, receipt lists and gossip. With reth's Ethereum types, one such message ended the peer's session.
  - A frame transaction keeps its bytes as received, so its hash and `transactionsRoot` are exact whatever the draft's layout. skim reads only what it uses: sender, nonce, EIP-8250 nonce keys, fees, and each frame's mode, target and gas. It still reads them if the frames change shape.
  - Receipts follow EIP-8141's encoding, `0x06 || rlp([cumulative_gas_used, payer, frames])` with no bloom, checked against `receiptsRoot`. A transaction's logs are its frames' logs in frame order. JSON receipts add `payer` and `frameReceipts`, as Hegota clients return them.

## Assumptions that will change
Each assumption is a placeholder for the final spec, not a design decision. Each has to be revisited as its EIP settles.

| Assumption | Why it's a guess | What would replace it |
|---|---|---|
| **Bogotá executes with Glamsterdam's EVM rules.** revm (43) has no Bogotá spec, so `eth_call`, `eth_estimateGas` and simulations after the fork run Amsterdam semantics. Applied state isn't affected: BAL post-values are the state. | Any opcode, gas or precompile change in Bogotá makes local results differ. Several such EIPs are under consideration. | a revm Bogotá `SpecId`, selected by block timestamp |
| **A frame transaction's `from` is its `sender` field.** It isn't recovered from a signature: VERIFY frames establish it during execution, which skim doesn't run. For a transaction in a canonical block, consensus has checked it. | EIP-8141 is a draft. | the final sender rule |
| **A frame receipt's `status` is 1 when no frame failed.** EIP-8141 receipts have no transaction-level status. | The JSON convention isn't specified. | the execution-apis spec for frame receipts |
| **Frame transactions can't be sent or traced through skim.** `eth_sendRawTransaction` refuses one, because skim can't check its validation frames before gossiping it. `debug_trace*` refuses one, because skim's EVM has no EIP-8141 rules. | Both need the frame execution rules. | EIP-8141 execution in revm |
| **The execution payload, envelope and BAL formats are Glamsterdam's.** | Heze drafts don't change them yet. EIP-8279 (BAL byte floor) and EIP-8116 (receipt fields) are under consideration. | per-fork parsers like the block body's |
| **The light-client header and its branch gindices are Gloas's.** | There is no Heze light-client spec yet. Progressive containers keep field indices stable when fields are added or removed, which is why this is likely to hold. | the Heze light-client spec |
| **One slot duration.** A Heze config that changes it (`SLOT_DURATION_MS_HEZE`, from older drafts, or EIP-8198 quick slots) is a stop. | skim's slot clock assumes a fixed duration from genesis. | a slot clock that changes duration at a fork |
| **skim takes no part in FOCIL.** It doesn't subscribe to `inclusion_list` gossip or serve `InclusionListsByIndices`. | A follower doesn't need inclusion lists to apply blocks. Serving them is part of [participating in the network](future-considerations.md#participating-in-the-network-not-just-consuming-it). | an inclusion-list role, if skim takes one |

## Tested
**On bogota-devnet-1** (2026-10-21; Bogotá on Gloas with EIP-8141, 8250, 8272 and 7906; Lighthouse with Geth, Nethermind, Besu, ethrex and reth):
- `skim run --network <its config dir> --experimental-hegota` follows the head. It connects to all five EL clients; their fork ids match, and they serve state over SNAP. State reads (balance, nonce, code) match the devnet's RPC, including for a frame-transaction sender.
- `eth_getLogs` over blocks with frame transactions works. Bodies and receipts decode, transaction hashes match the devnet's, and both roots verify.
- The devnet's own RPC disagreed with itself on a frame transaction whose last frame failed but whose third frame logged: queried per block it returned that log, over a range it didn't. skim returns it, per EIP-8141, which defines a transaction's logs as its frames' logs.
- Not yet: the light client. Lighthouse doesn't serve light-client data on Gloas yet, so `latest` stays optimistic there.

**With fixtures:**
- a real frame receipt from the devnet, against its block's `receiptsRoot`;
- frame transactions in the devnet's layout and in an unparseable future one;
- the Heze body layout, on a Sepolia block re-laid out as Heze;
- digests, ENR next-fork and signing versions across a Heze boundary;
- stop and follow decisions for Hegota, unknown forks and one-sided Hegota schedules;
- reth's fork id at a scheduled Bogotá.

**On Sepolia:** the same types serve receipts and logs unchanged.

A Heze devnet is the next check, once consensus clients ship it.

## Devnets
Point `--network` at a directory with the devnet's `config.yaml`, `genesis.ssz`, `genesis.json`, `bootstrap_nodes.txt` (ENRs) and `enodes.txt` ([configuration](configuration.md)). For a network from a directory, skim also dials the listed nodes directly. bogota-devnet-1's nodes stopped answering discovery from one address after many restarts, while their TCP ports stayed open.
