# Archiving history for your state

A full archive node keeps every historical value of every key on the chain. Most apps need history for a few contracts. skim can keep **every historical version of just your subscribed state**, on disk, with no re-execution:

```yaml
state:
  subscriptions:
    - name: weth-usdc-0.3%
      address: "0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50"
      account: true
      storage: ["0x0", "0x1", "0x2", "0x4"]
      retention: { mode: archive }
    - name: quote-1-weth                 # archives every key this call reads, too
      calls: [{ to: "0xEd1f…2FB3", data: "0xc6a5026a…" }]
      retention: { mode: archive }
```
```bash
mkdir -p data
skim run --config skim.yaml --archive data/my-app.redb --archive-backfill 1000
```

Then query any covered block, with any tool:
```bash
cast storage 0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50 0 --block 11918000 --rpc-url http://127.0.0.1:8545
cast call 0xEd1f…2FB3 "quoteExactInputSingle((address,address,uint256,uint24,uint160))" "(…)" --block 11918000 --rpc-url …
```
A historical `eth_call` runs entirely on archived keys, with that block's header and `BLOCKHASH`. If it reads anything that isn't archived, it fails with `-32002` and says how many keys were missing. It never falls back to a guess.

## How it works
- **Writes:** each block's BAL already lists every write with its new value, so skim stores a version **only when a key changes**. A key that never changes costs one entry.
- **Coverage:** a key is covered from the block it became known (subscribe time) onwards, within *segments* of consecutively processed blocks.
- **Released keys:** a key no subscription needs for 64 blocks stops being archived. It stays covered up to the release block only, and archiving it again starts new coverage.
- **Restarts:** after a restart, the gap is healed in the background from eth/71 BALs. Peers retain BALs for about 2 weeks.
- **Reorgs:** the archive is truncated exactly by a per-block journal.

## Backfill: history from before you subscribed
`--archive-backfill N` walks the BALs of the N blocks before the archive's first block, and extends coverage backwards:
- **A key not written anywhere in that range:** its value was the same for the whole range, because a BAL lists every write. It is covered from the start of the range.
- **A key written in the range:** it is covered from its **first write** onward. The value *before* that write appears in no BAL, so skim can't know it and refuses (`-32002`) rather than guess.

Backfill depends on a complete verified chain of headers and BALs still served by peers. At Sepolia's 12 s slots, roughly two weeks is on the order of 100k blocks; other networks have different slot durations. `--archive-backfill N` is a target, not a guarantee or an automatic clamp to the fork/retention boundary. Unavailable ranges can keep retrying. Start with a few thousand blocks. Verified live: 300 blocks backfilled, 102 historical reads matched Infura, 3 were correctly refused, 0 were wrong.

Inspect the per-address coverage-start summary with `skim_archiveCoverage([address])`. It reports `key`, `from` and `starts_at_write`; it does not enumerate every segment or healed gap. A successful historical read still requires coverage at that exact block. `--history local` stores headers and log matches separately and does not replace the selective state archive.

## What's covered, exactly
| Read at block B of key K | Answer |
|---|---|
| B inside a processed segment, K covered at B | the archived value |
| K was never subscribed (or not yet at B) | `-32002` |
| B before the backfill/subscription start, or in an unhealed gap | `-32002` |

Verified against Infura (archive): 90/90 historical reads across slots, balances and quotes; 51/51 inside healed restart gaps. Details: [M4](../experiments/m4-archive.md).
