# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
22. **Restart recovery without a SNAP peer that serves the head.** The startup BAL seeding
    (`0e2acfc`) runs, but the fourth soak still had a 265 s restart waiting for a SNAP peer, for
    a key it doesn't name. Done when subscription status says which keys are missing (so the next
    slow restart can be explained), and a 150-minute soak passes the recovery gate or explains
    each restart over it.
23. **Archived call subscriptions grow without bound.** Keys of an archived subscription are never
    released, so when a call's access set drifts the subscribed set only grows. It survives
    restarts and eventually hits `max_subscribed_keys`, after which the subscription is truncated
    and never ready (fourth soak: 60 held against 45 needed). Done when keys a call stops reading
    leave the limit (for example: release them and end their archive coverage at that block, so
    reads past it are refused rather than wrong), with a test that drifts the access set past
    the limit.
