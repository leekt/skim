# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
1. **Stream reorg test.** `skim_streamNext`'s `rewind` was checked by hand only (a fake cursor hash
   with one canonical `recent` entry). Done: an automated test drives a stream across a reorg
   (cursor on the abandoned fork, `recent` with stale and canonical entries) and asserts the
   rewind target and that no abandoned-fork logs are delivered after it.
2. **`eth_getLogs({blockHash})` under a reorg.** The scan refuses a different block at the
   requested height; done when a test forces a reorg between resolving the hash and scanning.
3. **Pending whole-storage downloads.** The store side is tested; the subscription code no longer
   watches the address before installation, untested. Done: a network-free test of subscription
   maintenance where downloads keep failing and BAL writes to the contract keep coming.
4. **Header-only misses don't wait for SNAP peers.** Done: a test with all state local, a
   `BLOCKHASH` miss, and no SNAP peer, asserting the call completes.
5. **Soak on the current build.** The last full soak (`c020e7d`) predates history, streams and the
   eighth/ninth review fixes, and failed freshness (old metric) and config-switch recovery (407 s,
   215 s). Done: a 150-minute soak on the current build, results in the experiment log, with any
   failure explained.

## Memory and disk
6. **Memory during a long catch-up.** skim reached 514 MB RSS catching up the post-fork history
   (113-124 MB following the head afterwards). Done: the cause identified and catch-up held near
   the head-following figure.

## Performance
8. **EVM execution on a bounded CPU pool,** without slowing short warm calls (measured).
9. **Reused per-run EVM setup; partial eviction for the bytecode-analysis cache** (it clears all
   4,096 entries when full).
10. **Peer-request counts** for request coalescing and completion-order fetching: metrics, and
    before/after numbers.

## Streams and modules
11. **BAL stream:** per-transaction state changes (keeping `block_access_index`), a separate hook
    from logs.
12. **Export separate from retention:** a module can receive changes for keys it doesn't make skim
    keep in memory.
13. **New-block push at the head:** the stream waits by polling (200 ms); notify instead, data
    still pulled by cursor.

## smallname
16. **Unsupported routes:** permissions by `name` / `registration_id` (needs `TokenResource`
    resource ids indexed), resolver `links` / `roles`, diagnostics, Basenames (another chain).

## Deployment
18. **No deployment example in the repo:** a reference setup for skim + smallname (service units,
    a reverse proxy or tunnel in front of smallname only, skim's RPC kept private).
