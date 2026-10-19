# Open work

What's known to be missing or unproven, as of 2026-10-19. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Networks other than Sepolia
25. **TCP peers that don't take yamux.** skim offers only yamux over TCP; a client that requires
    another muxer and announces no QUIC can't be peered with (Lodestar's devnet build refuses
    yamux; it announces QUIC, so skim uses that). Done when such a peer is either supported or
    shown not to exist on the networks skim targets.

## Memory
6. **Whether RSS levels off.** Nine production hours: RSS levelled off with the counted
   structures at ~7 h (346 MB at 9 h, +3 MB over the last two hours; growth was ~2.3x the counted
   payload, i.e. allocator and table overhead), see [hardware](guide/hardware.md#memory). Done
   when a 27 h run (headers full) is flat, measured on a build with `--tx-index pinned`. `memory_usage.md` (repo root, uncommitted) is a handoff on this for another agent.

The last items closed: 22, restart recovery without a SNAP peer, and 23, archived call
subscriptions growing without bound (sixth soak, M7 log); 26, diagnostic command scope:
`run` is network-configurable and the other diagnostic workloads are explicitly Sepolia-only
(`959adcf`).
