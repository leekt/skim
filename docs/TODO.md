# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
4. **Header-only misses don't wait for SNAP peers.** Covered for `eth_call` with no peers at all
   (`crates/skim-sim/tests/header_only_miss.rs`: bounded `Incomplete`, never `NoStatePeers`).
   Still untested: an eth-only peer supplying the header so the call completes, and the
   `eth_simulateV1` / trace paths (`multi.rs`), which have the same shortcut.
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

## smallname
16. **Unsupported routes:** permissions by `name` / `registration_id` (needs `TokenResource`
    resource ids indexed), resolver `links` / `roles`, diagnostics, Basenames (another chain).

## Deployment
18. **No deployment example in the repo:** a reference setup for skim + smallname (service units,
    a reverse proxy or tunnel in front of smallname only, skim's RPC kept private).
