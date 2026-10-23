# Open work

What's known to be missing or unproven, as of 2026-10-19. Each item says what "done" means.
Items are removed from this file when they're done, and the commit that closes one says so.

## Networks other than Sepolia
25. **TCP peers that don't take yamux.** skim offers only yamux over TCP; a client that requires
    another muxer and announces no QUIC can't be peered with (Lodestar's devnet build refuses
    yamux; it announces QUIC, so skim uses that). Done when such a peer is either supported or
    shown not to exist on the networks skim targets.
29. **Hegota beyond experimental.** `--experimental-hegota` follows Hegota under placeholder
    assumptions (Glamsterdam's EVM rules, the drafted Heze body, frame transactions decoded
    but not executed: [Hegota](guide/hegota.md#assumptions-that-will-change)). It follows
    bogota-devnet-1 (Bogotá on Gloas) already. Done when each assumption is replaced by the
    final spec's rule, skim has crossed a Heze fork on a devnet, and support is on by default.

## Measurements
27. **Hedged fetch under live stalls.** With injected stalls (mock peers, ~3% stalling 2 s) adaptive hedging cuts p99 from 1,233 to 187 ms at no extra request cost; on 3,600 live cold reads it cost nothing but no peer stalled, so the live benefit is unobserved (M7, "Hedged fetch"). The maximum stays ~1.2 s when a job's one hedge also stalls. Done when a run that includes stalls (a long soak; `skim_snap_fetch` hedges > 0) shows the tail with adaptive vs fixed.

## Releases
28. **Signed releases.** `install.sh` checks each tarball's SHA-256 from the same release, which catches corrupted downloads but not a compromised release. Done when release artifacts carry signatures or build provenance (e.g. GitHub artifact attestations) and the install docs say how to verify them.

## Memory
6. **Whether RSS levels off.** Mostly, with a slow tail. Production (pinned-index build, mimalloc)
   stayed ~370-400 MB from ~9 h through 27.7 h. A 43 h zero-config soak plateaued by ~6 h
   (~268 MB) and then crept ~1 MB/h to 307 MB, with what still fills (headers until ~27 h)
   accounting for a fraction of it ([hardware](guide/hardware.md#long-runs)). Production on
   jemalloc sits ~22% lower at 20 h. Done when a jemalloc run past 30 h shows whether the
   tail is allocator overhead (flat) or a structure the counters miss (still rising).

The last items closed: 24, a live blob-parameter fork crossed on the devnet without losing peers
(`162eb48`, `a27a7d7`); 26, diagnostic command scope: `run` is network-configurable and the other
diagnostic workloads are explicitly Sepolia-only (`959adcf`); 22, restart recovery without a SNAP
peer, and 23, archived call subscriptions growing without bound (sixth soak, M7 log).
