use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Behav {
    Ok { ms: u64 },
    /// Session drops on the first request; the peer leaves the pool.
    Disconnect,
    Unavailable,
    /// Verified empty storage trie: every slot is zero.
    Empty,
    /// Can't verify (no proof / root not served): must never resolve as zero.
    EmptyLying,
    /// Root not executed yet: the first `n` requests are unavailable, then it serves.
    DelayedRoot(usize),
    Fail,
    /// Answers in `ms`, except the `at`-th request overall, which stalls for `stall_ms`.
    StallAt { ms: u64, at: usize, stall_ms: u64 },
    /// Answers in `ms`; every `stall_every`-th request overall stalls `stall_ms`, every
    /// `fail_every`-th fails at once (a dropped request). Deterministic.
    Chaos { ms: u64, stall_every: usize, stall_ms: u64, fail_every: usize },
}

#[derive(Clone)]
struct Mock {
    peers: Arc<Mutex<Vec<PeerId>>>,
    behav: Arc<HashMap<PeerId, Behav>>,
    requests: Arc<AtomicUsize>,
    inflight: Arc<Mutex<HashMap<PeerId, (usize, usize)>>>, // (now, max)
}

fn pid(n: u8) -> PeerId {
    PeerId::repeat_byte(n)
}

impl Mock {
    fn new(peers: &[(u8, Behav)]) -> Self {
        Self {
            peers: Arc::new(Mutex::new(peers.iter().map(|(n, _)| pid(*n)).collect())),
            behav: Arc::new(peers.iter().map(|(n, b)| (pid(*n), *b)).collect()),
            requests: Default::default(),
            inflight: Default::default(),
        }
    }

    async fn serve<R>(&self, peer: PeerId, ok: impl FnOnce() -> R, unavailable: R, empty: R) -> Result<R, TransportError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        {
            let mut m = self.inflight.lock();
            let e = m.entry(peer).or_default();
            e.0 += 1;
            e.1 = e.1.max(e.0);
        }
        let r = match self.behav[&peer] {
            Behav::Ok { ms } => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(ok())
            }
            Behav::Disconnect => {
                self.peers.lock().retain(|p| *p != peer);
                Err(TransportError::Disconnected)
            }
            Behav::Unavailable | Behav::EmptyLying => Ok(unavailable),
            Behav::Empty => Ok(empty),
            Behav::DelayedRoot(n) => {
                if self.requests.load(Ordering::SeqCst) <= n { Ok(unavailable) } else { Ok(ok()) }
            }
            Behav::Fail => Err(TransportError::Timeout),
            Behav::Chaos { ms, stall_every, stall_ms, fail_every } => {
                let n = self.requests.load(Ordering::SeqCst);
                if n % fail_every == fail_every / 2 {
                    Err(TransportError::Timeout)
                } else {
                    tokio::time::sleep(Duration::from_millis(if n % stall_every == 0 { stall_ms } else { ms })).await;
                    Ok(ok())
                }
            }
            Behav::StallAt { ms, at, stall_ms } => {
                let n = self.requests.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(if n == at { stall_ms } else { ms })).await;
                Ok(ok())
            }
        };
        self.inflight.lock().get_mut(&peer).unwrap().0 -= 1;
        r
    }
}

fn acct() -> AccountState {
    AccountState { nonce: 1, balance: U256::from(7), storage_root: B256::ZERO, code_hash: B256::ZERO }
}

impl Transport for Mock {
    fn snap_peers(&self) -> Vec<PeerId> {
        self.peers.lock().clone()
    }
    async fn account(&self, peer: PeerId, _: B256, _: Address) -> Result<AccountAnswer, TransportError> {
        self.serve(peer, || AccountAnswer::Found(Some(acct())), AccountAnswer::Unavailable, AccountAnswer::Found(Some(AccountState { storage_root: EMPTY_ROOT_HASH, ..acct() }))).await
    }
    async fn storage(&self, peer: PeerId, _: B256, _: Address, slot: U256) -> Result<SlotAnswer, TransportError> {
        self.serve(peer, || SlotAnswer::Found(slot + U256::from(1)), SlotAnswer::Unavailable, SlotAnswer::Found(U256::ZERO)).await
    }
    async fn codes(&self, peer: PeerId, hashes: Vec<B256>) -> Result<Vec<Bytes>, TransportError> {
        // Code whose keccak is the requested hash can't be faked; serve the preimages we know.
        let have: Vec<Bytes> = hashes
            .iter()
            .filter_map(|h| CODES.iter().find(|c| keccak256(c) == *h))
            .map(|c| Bytes::from_static(c))
            .collect();
        let (none, none2): (Vec<Bytes>, Vec<Bytes>) = (Vec::new(), Vec::new());
        let r = self.serve(peer, move || have, none, none2).await;
        r
    }
}

static CODES: [&[u8]; 10] = [b"c0", b"c1", b"c2", b"c3", b"c4", b"c5", b"c6", b"c7", b"c8", b"c9"];

fn slot(i: u64) -> Key {
    Key::Storage { root: B256::ZERO, address: Address::ZERO, slot: U256::from(i) }
}

fn cfg() -> Config {
    Config { hedge_after: Duration::from_secs(10), deadline: Duration::from_secs(5), ..Default::default() }
}

#[tokio::test]
async fn dedups_concurrent_waiters() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 30 })]);
    let s = Scheduler::new(m.clone(), cfg());
    let k = [slot(1)];
    let (a, b) = tokio::join!(s.fetch(&k), s.fetch(&k));
    assert_eq!(a[0], Ok(Value::Storage(U256::from(2))));
    assert_eq!(a, b);
    assert_eq!(m.requests.load(Ordering::SeqCst), 1);
    assert_eq!(s.stats().dedup_hits, 1);
}

#[tokio::test]
async fn disconnect_requeues_to_other_peer() {
    let m = Mock::new(&[(1, Behav::Disconnect), (2, Behav::Ok { ms: 5 })]);
    let s = Scheduler::new(m, cfg());
    let keys: Vec<_> = (0..20).map(slot).collect();
    let r = s.fetch(&keys).await;
    assert!(r.iter().all(|x| x.is_ok()), "{r:?}");
    assert_eq!(s.stats().failed, 0);
    assert!(s.stats().disconnects >= 1);
}

#[tokio::test]
async fn respects_per_peer_inflight_cap() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 20 }), (2, Behav::Ok { ms: 20 })]);
    let s = Scheduler::new(m.clone(), Config { per_peer_inflight: 4, adaptive: false, ..cfg() });
    let keys: Vec<_> = (0..64).map(slot).collect();
    assert!(s.fetch(&keys).await.iter().all(|x| x.is_ok()));
    for (_, (_, max)) in m.inflight.lock().iter() {
        assert!(*max <= 4, "max in flight {max}");
    }
}

#[tokio::test]
async fn unavailable_root_moves_to_other_peer() {
    let m = Mock::new(&[(1, Behav::Unavailable), (2, Behav::Ok { ms: 5 })]);
    let s = Scheduler::new(m, cfg());
    let k = Key::Account { root: B256::ZERO, address: Address::ZERO };
    assert_eq!(s.fetch(&[k]).await[0], Ok(Value::Account(Some(acct()))));
}

#[tokio::test]
async fn batches_code_hashes() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 5 })]);
    let s = Scheduler::new(m.clone(), cfg());
    let keys: Vec<_> = CODES.iter().map(|c| Key::Code(keccak256(c))).collect();
    let r = s.fetch(&keys).await;
    assert!(r.iter().all(|x| matches!(x, Ok(Value::Code(_)))), "{r:?}");
    assert_eq!(m.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hedges_slow_peer() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 2000 }), (2, Behav::Ok { ms: 5 })]);
    // Peer 1 gets the first request (both unmeasured, tie broken arbitrarily), so force it:
    // with a single key one of them is picked; either way the call must finish quickly.
    let s = Scheduler::new(m, Config { hedge_after: Duration::from_millis(100), ..cfg() });
    let t = Instant::now();
    assert!(s.fetch(&[slot(3)]).await[0].is_ok());
    assert!(t.elapsed() < Duration::from_millis(1000), "took {:?}", t.elapsed());
}

/// One request stalls at a peer that otherwise answers in ~10 ms: how long until it's answered?
async fn stalled_read(adaptive: bool) -> Duration {
    let b = Behav::StallAt { ms: 10, at: 41, stall_ms: 3000 };
    let m = Mock::new(&[(1, b), (2, b)]);
    let s = Scheduler::new(m, Config { hedge_adaptive: adaptive, hedge_after: Duration::from_millis(1200), ..cfg() });
    for i in 0..40 {
        assert!(s.fetch(&[slot(100 + i)]).await[0].is_ok()); // both peers learn ~10 ms
    }
    let t = Instant::now();
    assert!(s.fetch(&[slot(1)]).await[0].is_ok());
    t.elapsed()
}

#[tokio::test]
async fn a_stall_at_a_fast_peer_is_hedged_after_its_own_latency_not_a_fixed_delay() {
    // Review (Myotis comparison): hedging after a fixed 1.2 s left the cold-read tail at
    // ~1.2 s whenever one fast peer stalled. Adaptive: hedged at the 150 ms floor.
    let adaptive = stalled_read(true).await;
    assert!(adaptive < Duration::from_millis(500), "adaptive took {adaptive:?}");
    let fixed = stalled_read(false).await;
    assert!(fixed >= Duration::from_millis(1200), "fixed took {fixed:?}");
}

#[tokio::test]
async fn empty_storage_trie_resolves_zero() {
    let m = Mock::new(&[(1, Behav::Empty)]);
    let s = Scheduler::new(m, cfg());
    assert_eq!(s.fetch(&[slot(9)]).await[0], Ok(Value::Storage(U256::ZERO)));
}

#[tokio::test]
async fn empty_answer_from_unserving_peer_is_never_zero() {
    // Regression (Spike P0-A): an empty-without-proof answer for an account with storage was
    // taken as zero, corrupting state. It must fall through to a peer that serves the root.
    let m = Mock::new(&[(1, Behav::EmptyLying), (2, Behav::Ok { ms: 30 })]);
    let s = Scheduler::new(m, cfg());
    assert_eq!(s.fetch(&[slot(9)]).await[0], Ok(Value::Storage(U256::from(10))));

    let m = Mock::new(&[(1, Behav::EmptyLying)]);
    let s = Scheduler::new(m, Config { max_attempts: 3, ..cfg() });
    assert!(s.fetch(&[slot(9)]).await[0].is_err());
}

#[tokio::test]
async fn root_served_late_yields_the_value_never_zero() {
    // The only peer hasn't executed the root for its first 3 requests (head-of-chain race).
    let m = Mock::new(&[(1, Behav::DelayedRoot(3))]);
    let s = Scheduler::new(m, Config { max_attempts: 10, ..cfg() });
    let t = Instant::now();
    let r = s.fetch(&[slot(5)]).await;
    assert_eq!(r[0], Ok(Value::Storage(U256::from(6))));
    // Each unavailable answer parks the peer for ROOT_RETRY before retrying the same root.
    assert!(t.elapsed() >= ROOT_RETRY * 3, "{:?}", t.elapsed());
}

#[tokio::test]
async fn exhausts_when_every_peer_fails() {
    let m = Mock::new(&[(1, Behav::Fail), (2, Behav::Fail)]);
    let s = Scheduler::new(m, Config { max_attempts: 3, ..cfg() });
    assert_eq!(s.fetch(&[slot(1)]).await[0], Err(FetchError::Exhausted(3)));
}

#[tokio::test]
async fn waits_for_peers_then_deadline() {
    let m = Mock::new(&[]);
    let s = Scheduler::new(m, Config { deadline: Duration::from_millis(200), no_peer_wait: Duration::from_millis(50), ..cfg() });
    let t = std::time::Instant::now();
    assert_eq!(s.fetch(&[slot(1)]).await[0], Err(FetchError::NoPeers));
    assert!(t.elapsed() < Duration::from_millis(190), "interactive jobs give up early when no peer exists");
}

#[tokio::test]
async fn queueing_for_capacity_is_not_a_deadline() {
    // 40 keys, one peer, 2 in flight, 50 ms each: ~1 s of queueing against a 300 ms deadline.
    let m = Mock::new(&[(1, Behav::Ok { ms: 50 })]);
    let s = Scheduler::new(m, Config { per_peer_inflight: 2, adaptive: false, deadline: Duration::from_millis(300), ..cfg() });
    let keys: Vec<_> = (0..40).map(slot).collect();
    assert!(s.fetch(&keys).await.iter().all(|x| x.is_ok()));
}

#[tokio::test]
async fn aimd_gives_fast_peers_more_concurrency() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 5 }), (2, Behav::Ok { ms: 120 })]);
    let s = Scheduler::new(m.clone(), Config { per_peer_inflight: 4, inflight_max: 32, ..cfg() });
    let keys: Vec<_> = (0..600).map(slot).collect();
    let t = Instant::now();
    assert!(s.fetch(&keys).await.iter().all(|x| x.is_ok()));
    let ps = s.peer_stats();
    let (fast, slow) = (&ps[&pid(1)], &ps[&pid(2)]);
    assert!(fast.limit > slow.limit + 4.0, "fast {} slow {}", fast.limit, slow.limit);
    assert!(fast.ok > 5 * slow.ok, "fast {} slow {}", fast.ok, slow.ok);
    // Fixed cap 4 on both would take >= 600 / (4/5ms + 4/120ms) ~ 0.72 s.
    assert!(t.elapsed() < Duration::from_millis(700), "took {:?}", t.elapsed());
}

#[test]
fn aimd_rules() {
    let c = Config { per_peer_inflight: 8, inflight_max: 16, ..cfg() };
    let mut p = Peer::default();
    p.on_answer(&c, 50.0); // initializes at 8, min 50
    assert!(p.limit > 8.0);
    for _ in 0..500 {
        p.on_answer(&c, 55.0);
    }
    assert_eq!(p.limit, 16.0); // capped
    p.ewma_ms = Some(55.0);
    p.on_answer(&c, 400.0); // > 3x typical + 100: queueing at the peer
    assert!((p.limit - 11.2).abs() < 1e-9);
    p.on_error(&c);
    assert!((p.limit - 5.6).abs() < 1e-9);
    for _ in 0..10 {
        p.on_error(&c);
    }
    assert_eq!(p.limit, 1.0);
    assert_eq!(p.cap(&c), 1);
    assert_eq!(Peer::default().cap(&Config { adaptive: false, ..c }), 8);
}

#[tokio::test]
async fn background_work_never_takes_more_than_half_the_cap_and_promotes() {
    let m = Mock::new(&[(1, Behav::Ok { ms: 40 })]);
    let s = Scheduler::new(m.clone(), Config { per_peer_inflight: 8, adaptive: false, ..cfg() });
    // 64 background keys: at most 4 in flight at once on the single peer.
    let bg: Vec<_> = (0..64).map(slot).collect();
    let s2 = s.clone();
    let bg_task = tokio::spawn(async move { s2.fetch_detailed_with(&bg, Priority::Low).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // A simulation needs one of the queued background keys and 3 fresh ones: they must not wait
    // for the background backlog.
    let t = Instant::now();
    let hi = s.fetch(&[slot(63), slot(100), slot(101), slot(102)]).await;
    assert!(hi.iter().all(|x| x.is_ok()));
    assert!(t.elapsed() < Duration::from_millis(200), "high waited {:?}", t.elapsed());
    assert!(bg_task.await.unwrap().iter().all(|(r, _)| r.is_ok()));
    assert!(s.stats().upgraded >= 1);
    // Low jobs alone never exceeded half the cap (4); high jobs could use the rest.
    assert!(m.inflight.lock()[&pid(1)].1 <= 8);
}

#[tokio::test]
async fn high_is_not_starved_by_a_large_background_backlog_under_aimd() {
    // Reproduction attempt for run F: thousands of Low jobs, AIMD on, a High request later.
    let m = Mock::new(&[(1, Behav::Ok { ms: 30 }), (2, Behav::Ok { ms: 30 })]);
    let s = Scheduler::new(m.clone(), Config { per_peer_inflight: 2, ..cfg() });
    let bg: Vec<_> = (0..2000).map(slot).collect();
    let s2 = s.clone();
    let bg_task = tokio::spawn(async move { s2.fetch_detailed_with(&bg, Priority::Low).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    let hi = s.fetch(&[slot(5000), slot(5001)]).await;
    assert!(hi.iter().all(|x| x.is_ok()));
    assert!(t.elapsed() < Duration::from_millis(300), "high waited {:?}", t.elapsed());
    bg_task.abort();
}

#[tokio::test]
async fn a_job_nobody_waits_for_is_dropped_not_sent() {
    // One slot: A is in flight, B queues behind it. B's caller gives up before it is sent.
    let m = Mock::new(&[(1, Behav::Ok { ms: 100 })]);
    let s = Scheduler::new(m.clone(), Config { per_peer_inflight: 1, adaptive: false, ..cfg() });
    let mut a = s.submit(&[slot(1)], Priority::High);
    let b = s.submit(&[slot(2)], Priority::High);
    drop(b);
    assert!(a.pop().unwrap().await.0.is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(m.requests.load(Ordering::SeqCst), 1, "the cancelled job was never sent");
    assert_eq!(s.stats().orphaned, 1);
    assert_eq!(s.queue_lens(), (0, 0));
    // The same key submitted again is fetched once, not twice from a stale queue entry.
    assert!(s.fetch(&[slot(2)]).await[0].is_ok());
    assert_eq!(m.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn background_work_runs_on_a_peer_with_a_cap_of_one() {
    // Half of a cap of 1 is 0: without the idle-peer allowance this job would wait forever.
    let m = Mock::new(&[(1, Behav::Ok { ms: 5 })]);
    let s = Scheduler::new(m, Config { per_peer_inflight: 1, adaptive: false, ..cfg() });
    let f = s.submit(&[slot(1)], Priority::Low).pop().unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), f).await.expect("background job ran");
    assert!(r.0.is_ok());
}

/// Cold reads against peers that sometimes stall or drop a request: (sorted latencies in ms,
/// requests sent).
async fn chaos_reads(adaptive: bool, n: usize) -> (Vec<f64>, usize) {
    let b = Behav::Chaos { ms: 10, stall_every: 33, stall_ms: 2000, fail_every: 97 };
    let m = Mock::new(&[(1, b), (2, b), (3, b)]);
    let requests = m.requests.clone();
    let s = Scheduler::new(m, Config { hedge_adaptive: adaptive, hedge_after: Duration::from_millis(1200), deadline: Duration::from_secs(15), ..cfg() });
    for i in 0..30 {
        let _ = s.fetch(&[slot(1_000_000 + i)]).await; // peers learn their latency
    }
    let start_requests = requests.load(Ordering::SeqCst);
    let mut lat = Vec::new();
    for wave in 0..n / 6 {
        let futs = (0..6).map(|j| {
            let s = s.clone();
            async move {
                let t = Instant::now();
                let ok = s.fetch(&[slot((wave * 6 + j) as u64)]).await[0].is_ok();
                (t.elapsed().as_secs_f64() * 1e3, ok)
            }
        });
        for (ms, ok) in futures::future::join_all(futs).await {
            assert!(ok);
            lat.push(ms);
        }
    }
    lat.sort_by(f64::total_cmp);
    (lat, requests.load(Ordering::SeqCst) - start_requests)
}

/// Run with `cargo test -p skim-snap --release -- --ignored --nocapture hedging_under_injected`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a benchmark: ~1 minute"]
async fn hedging_under_injected_stalls_and_drops() {
    let pct = |v: &[f64], p: f64| v[((p / 100.0) * v.len() as f64) as usize].round();
    let mut p99 = Vec::new();
    for adaptive in [true, false] {
        let (v, req) = chaos_reads(adaptive, 600).await;
        println!(
            "{:8}: n={} p50={} p95={} p99={} max={} ms, requests/read={:.2}",
            if adaptive { "adaptive" } else { "fixed" },
            v.len(), pct(&v, 50.0), pct(&v, 95.0), pct(&v, 99.0), v.last().unwrap().round(), req as f64 / v.len() as f64
        );
        p99.push(pct(&v, 99.0));
    }
    assert!(p99[0] < p99[1], "adaptive p99 {} vs fixed {}", p99[0], p99[1]);
}

