# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
22. **Restart recovery without a SNAP peer that serves the head: confirm in a soak.** Two soaks
    missed the 180 s recovery gate (245 s, 235 s) on a key subscribed at restart for an account
    that had just transacted, waiting for a SNAP peer. Since `0e2acfc` the BALs of the 32 blocks
    before the start are indexed and such keys are promoted from them (unit-tested; 31 blocks in
    1.7 s live). Done when a 150-minute soak on a build with it passes the recovery gate, or any
    restart over the gate is explained by something else.
