# Open work

What's known to be missing or unproven, as of 2026-10-18. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Correctness and tests
22. **Restart recovery: confirm on the current build.** The fifth soak (`01fd4fc`) passed the
    recovery gate (18 restarts, 16-116 s), and subscription status now names missing keys. Done
    with the next soak that passes the recovery gate on a build with the archive-reload fix.
23. **Archived call subscriptions grow without bound.** Keys of an archived subscription are never
    released, so when a call's access set drifts the subscribed set only grows. It survives
    restarts and eventually hits `max_subscribed_keys`, after which the subscription is truncated
    and never ready (fourth soak: 60 held against 45 needed). Done when keys a call stops reading
    leave the limit (for example: release them and end their archive coverage at that block, so
    reads past it are refused rather than wrong), with a test that drifts the access set past
    the limit. Release and coverage ends are in `01fd4fc`; archive keys reloaded at start no
    longer take slots (after the fifth soak). Done when a soak passes without samples over the
    key limit.
