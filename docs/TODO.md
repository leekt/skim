# Open work

What's known to be missing or unproven, as of 2026-10-19. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Networks other than Sepolia
25. **TCP peers that don't take yamux.** skim offers only yamux over TCP; a client that requires
    another muxer and announces no QUIC can't be peered with (Lodestar's devnet build refuses
    yamux; it announces QUIC, so skim uses that). Done when such a peer is either supported or
    shown not to exist on the networks skim targets.

## Memory
6. **Whether RSS levels off.** Three production hours: block caches and the window level off within
   an hour, the transaction index grows as designed until ~7 h, and the uncounted part of RSS still
   rose ~10 MB in the last 75 minutes ([hardware](guide/hardware.md#memory)). Done when a run past
   7 h (then 27 h) shows RSS flat once every counted structure is full, or the growth is
   attributed. `memory_usage.md` (repo root, uncommitted) is a handoff on this for another agent.

The last items closed: 22, restart recovery without a SNAP peer, and 23, archived call
subscriptions growing without bound (sixth soak, M7 log); 26, diagnostic command scope:
`run` is network-configurable and the other diagnostic workloads are explicitly Sepolia-only
(`959adcf`).
