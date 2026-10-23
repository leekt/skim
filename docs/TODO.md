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

## From the 2026-10-23 review
The [review](REVIEW.md) lists every finding; these are the open ones that need design work.

30. **Archive coverage as intervals.** Resubscribing an archived key replaces its coverage
    start, so history recorded before the release becomes unreadable (REVIEW C9). Done when a
    key's coverage is a set of intervals, reads inside an earlier interval are answered and the
    gap between intervals is refused.
31. **No disk sync under the store lock.** Archive commits (`write_block`, `put_code`,
    `write_base`) fsync while the store's write lock is held, stalling every read for a disk
    sync per block (REVIEW C4). Done when archive and memory still change together but reads
    never wait on archive I/O, and a slow-disk soak shows no read stalls per block.
32. **Browser access to the RPC.** The default `--cors *` and WebSocket upgrades without an
    `Origin` check let any web page read `skim_status`/`skim_dashboard` (watched addresses,
    peer ids) from a local node (REVIEW R6). Done when `skim_*` methods (or all methods) refuse
    cross-origin callers unless allowed, WebSocket checks `Origin` against `--cors`, and the
    default is decided and documented.
33. **Readiness as a policy.** Several unusable states only warn while `skim status` exits 0.
    Done when a readiness check takes required subscriptions, a maximum head age and required
    authentication (`--require-verified`), and fails when they aren't met.
34. **Whole-storage pages proven as they arrive.** `storage_all` checks only the final root, so
    a peer can stream junk until the end (with `max_slots: unlimited`, without bound) (REVIEW
    N16). Done when each page's range proof is checked against the storage root and the
    download stops at the first bad page.
35. **Per-request fetch budget.** One cold call can need thousands of SNAP fetches across its
    rounds (REVIEW R12; predicted waves are now capped, miss rounds aren't). Done when a
    request's total fetches are bounded and measured under a cold-burst load.

## Memory
6. **Whether RSS levels off.** Mostly, with a slow tail. Production (`--tx-index subscribed` build, mimalloc)
   stayed ~370-400 MB from ~9 h through 27.7 h. A 43 h zero-config soak plateaued by ~6 h
   (~268 MB) and then crept ~1 MB/h to 307 MB, with what still fills (headers until ~27 h)
   accounting for a fraction of it ([hardware](guide/hardware.md#long-runs)). Production on
   jemalloc sits ~22% lower at 20 h. Done when a jemalloc run past 30 h shows whether the
   tail is allocator overhead (flat) or a structure the counters miss (still rising).

The last items closed: 24, a live blob-parameter fork crossed on the devnet without losing peers
(`162eb48`, `a27a7d7`); 26, diagnostic command scope: `run` is network-configurable and the other
diagnostic workloads are explicitly Sepolia-only (`959adcf`); 22, restart recovery without a SNAP
peer, and 23, archived call subscriptions growing without bound (sixth soak, M7 log).
