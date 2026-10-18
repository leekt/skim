# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
21. **Deep header walks right after start.** On a fresh `--history local` instance, a stream
    2,600 blocks below the head failed in about half of the runs with `-32002 headers below N not
    served by peers` after ~600 headers. The four peers asked didn't return a run linked to the
    oldest header, and 30 s of client retries weren't enough
    ([measurement](experiments/m7-gates-and-acquisition.md#peer-requests-coalescing-and-fetch-order)).
    Done when the walk tries further peers, or keeps retrying while it makes progress, and the same
    request succeeds within a minute of start in repeated runs.
22. **Restart recovery without a SNAP peer that serves the head.** Two soaks in a row missed the
    180 s recovery gate on exactly this: the chain was followed within seconds, but keys the restart
    snapshot didn't hold waited for a SNAP peer serving the current root (245 s with `5237bc1`,
    235 s with `be3f0fa`; the only SNAP-capable peer, a Besu node, answered "root unavailable"). Done when the
    snapshot keeps every subscribed key, so no restart needs SNAP, or peer acquisition prefers
    peers that serve recent roots; and a 150-minute soak passes the recovery gate.

## Memory and disk
6. **Memory: head-only steady state and long catch-up.** The third soak (`be3f0fa`, `--history
   off`, 150 min) had RSS median 181 MB, max 263 MB, against at most 134 MB for `5237bc1`: over the
   130-160 MB head-only target. Between them came mimalloc, the stream/history code and the CPU
   pool; first find which (an A/B soak of `be3f0fa` with the system allocator).
   Catch-up, measured on a 2-core aarch64 Linux host over ~40k blocks: glibc 394-610 MB RSS while heaptrack showed a 228 MB peak live heap (freed receipt buffers
   kept in per-thread arenas); with mimalloc 225-375 MB; with mimalloc and scans no longer
   keeping the bodies they fetch, 230-296 MB. Following the head with mimalloc: 181 MB after
   10 minutes. Done when head-only RSS is back in 130-160 MB over a full soak, and catch-up stays
   near the head figure. For catch-up, the remaining gap is receipts in
   flight (a 256-block chunk; 64 measured 220-235 MB but slower) and live-heap attribution,
   which needs a symbolized heap profile (heaptrack couldn't unwind the release binary).
