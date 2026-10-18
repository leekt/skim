# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
5. **Soak on the current build.** The last full soak (`c020e7d`) predates history, streams and the
   eighth/ninth review fixes, and failed freshness (old metric) and config-switch recovery (407 s,
   215 s). Done: a 150-minute soak on the current build, results in the experiment log, with any
   failure explained.

## Memory and disk
6. **Memory during a long catch-up.** skim reached 514 MB RSS catching up the post-fork history
   (113-124 MB following the head afterwards). Done: the cause identified and catch-up held near
   the head-following figure.

## Performance
10. **Peer-request counts: before/after numbers.** Counters exist (`skim_peer_requests_total{kind}`
    and `skim_status.peer_requests`: headers, bodies, receipts, block access lists, SNAP account /
    storage / code). Done when request coalescing and completion-order fetching each have a
    measured before/after on a fixed workload.

## smallname
16. **Unsupported routes:** permissions by `registration_id`, resolver `links` / `roles`,
    diagnostics, Basenames (another chain). (Permissions by `name` work since `TokenResource` is
    indexed; existing indexes need a re-index for older resource ids.)
