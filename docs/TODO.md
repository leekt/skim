# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
5. **Soak on the current build.** The last full soak (`c020e7d`) predates history, streams and the
   eighth/ninth review fixes, and failed freshness (old metric) and config-switch recovery (407 s,
   215 s). Done: a 150-minute soak on the current build, results in the experiment log, with any
   failure explained.
21. **Deep header walks right after start.** On a fresh `--history local` instance, a stream
    2,600 blocks below the head failed in about half of the runs with `-32002 headers below N not
    served by peers` after ~600 headers. The four peers asked didn't return a run linked to the
    oldest header, and 30 s of client retries weren't enough
    ([measurement](experiments/m7-gates-and-acquisition.md#peer-requests-coalescing-and-fetch-order)).
    Done when the walk tries further peers, or keeps retrying while it makes progress, and the same
    request succeeds within a minute of start in repeated runs.

## Memory and disk
6. **Memory during a long catch-up.** Measured on a 2-core aarch64 Linux host, catching up ~40k blocks:
   glibc 394-610 MB RSS while heaptrack showed a 228 MB peak live heap (freed receipt buffers
   kept in per-thread arenas); with mimalloc 225-375 MB; with mimalloc and scans no longer
   keeping the bodies they fetch, 230-296 MB. Following the head with mimalloc: 181 MB after
   10 minutes. Done when catch-up stays near the head figure: the remaining gap is receipts in
   flight (a 256-block chunk; 64 measured 220-235 MB but slower) and live-heap attribution,
   which needs a symbolized heap profile (heaptrack couldn't unwind the release binary).
