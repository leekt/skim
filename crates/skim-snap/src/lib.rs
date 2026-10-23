//! Adaptive SNAP scheduler (PROJECT.md §6.3.1, §7.2).
//!
//! The scheduler, not a peer session, owns every request:
//! - **Dedup:** one job per `(root, key)`. Concurrent waiters share the result.
//! - **Placement:** the least-loaded fast peer, with a per-peer in-flight cap.
//! - **Recovery:** on a disconnect, timeout or "root unavailable", the job is re-queued for
//!   another peer. Nothing is lost when a session drops.
//! - **Hedging:** a job that is slow on one peer gets one duplicate on another. The first
//!   answer wins and later answers are ignored.
//! - **Batching:** queued code hashes go out together in one `GetByteCodes`.
//!
//! Network access sits behind [`Transport`], so the policy is testable without peers.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use parking_lot::Mutex;
use tokio::sync::{oneshot, Notify};

pub mod el;
pub mod proof;

pub type PeerId = alloy_primitives::B512;

/// Root of an empty Merkle-Patricia trie: `keccak256(rlp(""))`.
pub const EMPTY_ROOT_HASH: B256 =
    alloy_primitives::b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");

/// What a lookup asks for. Account and storage keys are pinned to a state root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Account { root: B256, address: Address },
    Storage { root: B256, address: Address, slot: U256 },
    Code(B256),
}

impl Key {
    fn root(&self) -> Option<B256> {
        match *self {
            Key::Account { root, .. } | Key::Storage { root, .. } => Some(root),
            Key::Code(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountState {
    pub nonce: u64,
    pub balance: U256,
    pub storage_root: B256,
    pub code_hash: B256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `None` = absent at that root (trusted unless proofs are verified, §8).
    Account(Option<AccountState>),
    Storage(U256),
    Code(Bytes),
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("gave up after {0} attempts")]
    Exhausted(u32),
    #[error("deadline exceeded")]
    Deadline,
    #[error("no SNAP peers connected")]
    NoPeers,
}

/// A peer's answer to a point lookup.
#[derive(Debug, Clone)]
pub enum AccountAnswer {
    /// The peer can't serve the root (empty response without proof).
    Unavailable,
    Found(Option<AccountState>),
}

#[derive(Debug, Clone)]
pub enum SlotAnswer {
    /// The peer couldn't (verifiably) answer: root not served yet, missing or bad proof. Never
    /// a value.
    Unavailable,
    /// Verified value; zero only when proven (absent account, empty storage trie, or an
    /// exclusion proof).
    Found(U256),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    #[error("timeout")]
    Timeout,
    #[error("peer gone")]
    Disconnected,
    #[error("{0}")]
    Other(String),
}

pub trait Transport: Clone + Send + Sync + 'static {
    /// Peers that can currently serve SNAP requests.
    fn snap_peers(&self) -> Vec<PeerId>;
    fn account(
        &self,
        peer: PeerId,
        root: B256,
        address: Address,
    ) -> impl Future<Output = Result<AccountAnswer, TransportError>> + Send;
    fn storage(
        &self,
        peer: PeerId,
        root: B256,
        address: Address,
        slot: U256,
    ) -> impl Future<Output = Result<SlotAnswer, TransportError>> + Send;
    /// Returns whatever subset of `hashes` the peer had.
    fn codes(
        &self,
        peer: PeerId,
        hashes: Vec<B256>,
    ) -> impl Future<Output = Result<Vec<Bytes>, TransportError>> + Send;
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Fixed per-peer in-flight cap when `adaptive` is off; the starting cap when it is on.
    pub per_peer_inflight: usize,
    /// AIMD per-peer concurrency: grow on fast answers, shrink on latency inflation or errors.
    pub adaptive: bool,
    pub inflight_max: usize,
    pub max_attempts: u32,
    /// Send one duplicate to another peer if no answer after this long. With `hedge_adaptive`,
    /// the upper bound of the per-peer threshold.
    pub hedge_after: Duration,
    /// Hedge at the asked peer's own typical latency plus `hedge_k` deviations (clamped to
    /// `hedge_floor..=hedge_after`), instead of a fixed `hedge_after`: a request stuck at a peer
    /// that usually answers in 15 ms goes to another one after ~150 ms, not 1.2 s.
    pub hedge_adaptive: bool,
    pub hedge_k: f64,
    pub hedge_floor: Duration,
    /// Hedges are skipped once they exceed this share of requests (when every peer is slow,
    /// duplicates only add load).
    pub hedge_budget: f64,
    pub deadline: Duration,
    /// How long an interactive job waits while no SNAP peer is connected at all (e.g. the first
    /// minute after a cold start) before failing with [`FetchError::NoPeers`].
    pub no_peer_wait: Duration,
    pub code_batch: usize,
    /// Use only the N peers with the best latency (experiments; `None` = all).
    pub max_peers: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            per_peer_inflight: 8,
            adaptive: true,
            inflight_max: 48,
            max_attempts: 8,
            hedge_after: Duration::from_millis(1200),
            hedge_adaptive: true,
            hedge_k: 4.0,
            hedge_floor: Duration::from_millis(150),
            hedge_budget: 0.1,
            deadline: Duration::from_secs(15),
            no_peer_wait: Duration::from_secs(3),
            code_batch: 32,
            max_peers: None,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Stats {
    pub jobs: u64,
    pub dedup_hits: u64,
    pub requests: u64,
    pub ok: u64,
    pub root_unavailable: u64,
    pub errors: u64,
    pub disconnects: u64,
    pub requeued: u64,
    pub hedges: u64,
    pub late_answers_ignored: u64,
    /// Answers that held none of the keys asked for (a peer without the code): not rewarded.
    pub empty_answers: u64,
    pub failed: u64,
    pub code_batches: u64,
    pub low_jobs: u64,
    /// Requests sent for Low (background) jobs; `requests` counts all.
    pub requests_low: u64,
    pub upgraded: u64,
    /// Jobs dropped because every caller had stopped waiting.
    pub orphaned: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PeerStats {
    pub limit: f64,
    pub min_ms: f64,
    pub ok: u64,
    pub errors: u64,
    pub ewma_ms: f64,
    pub inflight: usize,
}

/// Who is waiting: a simulation (`High`) or a background prefetch (`Low`). Low jobs use at most
/// half of each peer's in-flight cap and never get hedged, so they can't delay High jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    #[default]
    High,
    Low,
}

/// Where a job's time went (PROJECT.md §6.3 tail-latency breakdown).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FetchMeta {
    /// Enqueue to first send: waiting for peers or for per-peer capacity.
    pub queue_ms: f64,
    /// Enqueue to answer.
    pub total_ms: f64,
    pub attempts: u32,
    /// "Root unavailable" or empty-without-proof answers before success.
    pub unavailable: u32,
    pub hedged: bool,
    /// Shared another waiter's in-flight job.
    pub deduped: bool,
}

type Waiter = oneshot::Sender<(Result<Value, FetchError>, FetchMeta)>;

struct Job {
    low: bool,
    waiters: Vec<(Waiter, bool)>,
    attempts: u32,
    unavailable: u32,
    inflight: u32,
    tried: HashSet<PeerId>,
    /// The peer the latest request went to (whose latency decides when to hedge).
    last_peer: Option<PeerId>,
    created: Instant,
    first_sent: Option<Instant>,
    last_sent: Option<Instant>,
    hedged: bool,
    queued: bool,
}

#[derive(Default)]
struct Peer {
    inflight: usize,
    /// AIMD concurrency limit (0 = not initialized yet).
    limit: f64,
    min_ms: Option<f64>,
    ewma_ms: Option<f64>,
    /// Mean absolute deviation of answer times around `ewma_ms` (EWMA).
    dev_ms: Option<f64>,
    ok: u64,
    errors: u64,
    /// Roots this peer couldn't serve, and when. Retried after `ROOT_RETRY`: at the head a peer
    /// may simply not have executed the block yet.
    unavailable_roots: HashMap<B256, Instant>,
}

const ROOT_RETRY: Duration = Duration::from_millis(400);

impl Peer {
    fn cap(&self, cfg: &Config) -> usize {
        if cfg.adaptive && self.limit > 0.0 { self.limit.floor().max(1.0) as usize } else { cfg.per_peer_inflight }
    }

    /// Additive increase on answers near this peer's best latency; multiplicative decrease on
    /// latency inflation (queueing at the peer) and on errors.
    fn on_answer(&mut self, cfg: &Config, ms: f64) {
        if !cfg.adaptive {
            return;
        }
        if self.limit == 0.0 {
            self.limit = cfg.per_peer_inflight as f64;
        }
        let min = self.min_ms.map_or(ms, |m| m.min(ms));
        self.min_ms = Some(min);
        // Compare against the peer's typical latency, not its best: verified lookups mix one-
        // and two-request answers, and a min-based test read that spread as queueing.
        let typical = self.ewma_ms.unwrap_or(ms);
        if ms <= 1.5 * typical + 25.0 {
            self.limit = (self.limit + 1.0 / self.limit).min(cfg.inflight_max as f64);
        } else if ms > 3.0 * typical + 100.0 {
            self.limit = (self.limit * 0.7).max(1.0);
        }
    }

    fn on_error(&mut self, cfg: &Config) {
        if cfg.adaptive {
            let cur = if self.limit == 0.0 { cfg.per_peer_inflight as f64 } else { self.limit };
            self.limit = (cur * 0.5).max(1.0);
        }
    }

    fn score(&self) -> f64 {
        // Unmeasured peers look average-fast so they get tried.
        self.ewma_ms.unwrap_or(300.0) * (1.0 + self.inflight as f64)
    }
}

#[derive(Default)]
struct State {
    jobs: HashMap<Key, Job>,
    queue: VecDeque<Key>,
    queue_lo: VecDeque<Key>,
    peers: HashMap<PeerId, Peer>,
    stats: Stats,
}

pub struct Scheduler<T: Transport> {
    inner: Arc<Inner<T>>,
}

impl<T: Transport> Clone for Scheduler<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

struct Inner<T> {
    transport: T,
    cfg: Config,
    /// `cfg.hedge_adaptive`, switchable while running (A/B measurements on one node).
    hedge_adaptive: std::sync::atomic::AtomicBool,
    state: Mutex<State>,
    wake: Notify,
}

impl<T: Transport> Scheduler<T> {
    /// Spawns the dispatcher. Must be called inside a Tokio runtime.
    pub fn new(transport: T, cfg: Config) -> Self {
        let hedge_adaptive = std::sync::atomic::AtomicBool::new(cfg.hedge_adaptive);
        let inner = Arc::new(Inner { transport, cfg, hedge_adaptive, state: Mutex::default(), wake: Notify::new() });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                // Holds a strong ref for at most one tick, so dropping the scheduler stops this.
                let Some(inner) = weak.upgrade() else { return };
                Inner::dispatch(&inner);
                let _ = tokio::time::timeout(Duration::from_millis(25), inner.wake.notified()).await;
            }
        });
        Self { inner }
    }

    /// Fetches a wave of keys concurrently. Results are in input order.
    pub async fn fetch(&self, keys: &[Key]) -> Vec<Result<Value, FetchError>> {
        self.fetch_detailed(keys).await.into_iter().map(|(r, _)| r).collect()
    }

    /// Like [`Self::fetch`], with per-key timing and retry metadata.
    pub async fn fetch_detailed(&self, keys: &[Key]) -> Vec<(Result<Value, FetchError>, FetchMeta)> {
        self.fetch_detailed_with(keys, Priority::High).await
    }

    pub async fn fetch_detailed_with(&self, keys: &[Key], prio: Priority) -> Vec<(Result<Value, FetchError>, FetchMeta)> {
        futures::future::join_all(self.submit(keys, prio)).await
    }

    /// Enqueues `keys` now and returns one future per key (in input order) that resolves when
    /// that key's result is in. Unlike [`Self::fetch_detailed_with`], callers can act on each
    /// result as it completes (e.g. with `FuturesUnordered`) and submit follow-up keys (an
    /// account's bytecode) without waiting for the rest of the wave.
    pub fn submit(&self, keys: &[Key], prio: Priority) -> Vec<impl std::future::Future<Output = (Result<Value, FetchError>, FetchMeta)> + Send + 'static> {
        let rxs: Vec<_> = {
            let mut st = self.inner.state.lock();
            keys.iter().map(|k| self.inner.enqueue(&mut st, *k, prio)).collect()
        };
        self.inner.wake.notify_one();
        rxs.into_iter().map(|rx| async move { rx.await.unwrap_or((Err(FetchError::Deadline), FetchMeta::default())) }).collect()
    }

    /// (queued High jobs, queued Low jobs).
    pub fn queue_lens(&self) -> (usize, usize) {
        let st = self.inner.state.lock();
        (st.queue.len(), st.queue_lo.len())
    }

    /// Switches between adaptive and fixed hedging while running (A/B measurements).
    pub fn set_hedge_adaptive(&self, on: bool) {
        self.inner.hedge_adaptive.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn transport(&self) -> &T {
        &self.inner.transport
    }

    pub fn stats(&self) -> Stats {
        self.inner.state.lock().stats.clone()
    }

    pub fn peer_stats(&self) -> HashMap<PeerId, PeerStats> {
        self.inner
            .state
            .lock()
            .peers
            .iter()
            .map(|(id, p)| {
                (*id, PeerStats {
                    limit: p.limit,
                    min_ms: p.min_ms.unwrap_or(0.0),
                    ok: p.ok,
                    errors: p.errors,
                    ewma_ms: p.ewma_ms.unwrap_or(0.0),
                    inflight: p.inflight,
                })
            })
            .collect()
    }
}

impl<T: Transport> Inner<T> {
    fn enqueue(&self, st: &mut State, key: Key, prio: Priority) -> oneshot::Receiver<(Result<Value, FetchError>, FetchMeta)> {
        let (tx, rx) = oneshot::channel();
        if let Some(job) = st.jobs.get_mut(&key) {
            job.waiters.push((tx, true));
            st.stats.dedup_hits += 1;
            // A simulation now waits on a background job: promote it.
            if prio == Priority::High && job.low {
                job.low = false;
                let queued = job.queued;
                st.stats.upgraded += 1;
                if queued {
                    st.queue_lo.retain(|k| *k != key);
                    st.queue.push_front(key);
                }
            }
            return rx;
        }
        st.jobs.insert(
            key,
            Job {
                low: prio == Priority::Low,
                waiters: vec![(tx, false)],
                attempts: 0,
                unavailable: 0,
                inflight: 0,
                tried: HashSet::new(),
                last_peer: None,
                created: Instant::now(),
                first_sent: None,
                last_sent: None,
                hedged: false,
                queued: true,
            },
        );
        if prio == Priority::Low {
            st.queue_lo.push_back(key);
            st.stats.low_jobs += 1;
        } else {
            st.queue.push_back(key);
        }
        st.stats.jobs += 1;
        rx
    }

    fn finish(st: &mut State, key: &Key, result: Result<Value, FetchError>) {
        if let Some(job) = st.jobs.remove(key) {
            if result.is_err() {
                st.stats.failed += 1;
            }
            let now = Instant::now();
            let meta = FetchMeta {
                queue_ms: job.first_sent.unwrap_or(now).duration_since(job.created).as_secs_f64() * 1e3,
                total_ms: now.duration_since(job.created).as_secs_f64() * 1e3,
                attempts: job.attempts,
                unavailable: job.unavailable,
                hedged: job.hedged,
                deduped: false,
            };
            for (w, deduped) in job.waiters {
                let _ = w.send((result.clone(), FetchMeta { deduped, ..meta }));
            }
        }
    }

    fn requeue(st: &mut State, key: Key) {
        if let Some(job) = st.jobs.get_mut(&key) {
            if !job.queued && job.inflight == 0 {
                job.queued = true;
                if job.low { st.queue_lo.push_back(key) } else { st.queue.push_front(key) }
                st.stats.requeued += 1;
            }
        }
    }

    fn pick_peer(&self, st: &State, key: &Key, job: &Job) -> Option<PeerId> {
        let root = key.root();
        let allowed: Option<HashSet<PeerId>> = self.cfg.max_peers.map(|n| {
            let mut v: Vec<_> = st.peers.iter().map(|(id, p)| (*id, p.ewma_ms.unwrap_or(300.0))).collect();
            v.sort_by(|a, b| a.1.total_cmp(&b.1));
            v.into_iter().take(n).map(|(id, _)| id).collect()
        });
        let eligible = |(id, p): &(&PeerId, &Peer)| {
            allowed.as_ref().is_none_or(|a| a.contains(*id))
                && p.inflight < if job.low {
                    // Background work keeps at least one slot free for simulations, except on an
                    // idle peer with no simulation waiting: at an adaptive cap of 1 the half-cap
                    // rule is 0, and background jobs would otherwise never run.
                    let half = (p.cap(&self.cfg) / 2).min(p.cap(&self.cfg).saturating_sub(1));
                    if half == 0 && p.inflight == 0 && st.queue.is_empty() { 1 } else { half }
                } else {
                    p.cap(&self.cfg)
                }
                && root.is_none_or(|r| p.unavailable_roots.get(&r).is_none_or(|t| t.elapsed() > ROOT_RETRY))

        };
        let best = |untried_only: bool| {
            st.peers
                .iter()
                .filter(eligible)
                .filter(|(id, _)| !untried_only || !job.tried.contains(*id))
                .min_by(|a, b| a.1.score().total_cmp(&b.1.score()))
                .map(|(id, _)| *id)
        };
        best(true).or_else(|| best(false))
    }

    fn dispatch(self: &Arc<Self>) {
        let mut st = self.state.lock();

        // Sync the peer table. In-flight requests to vanished peers fail and get re-queued.
        let live: HashSet<PeerId> = self.transport.snap_peers().into_iter().collect();
        st.peers.retain(|id, _| live.contains(id));
        for id in live {
            st.peers.entry(id).or_default();
        }

        // Jobs nobody waits for any more (callers timed out or were cancelled): drop closed
        // waiters, and drop the job unless a request for it is in flight (that one finishes and
        // keeps the peer accounting right). Their queue entries are skipped when placed.
        let mut orphaned = Vec::new();
        for (k, j) in st.jobs.iter_mut() {
            j.waiters.retain(|(w, _)| !w.is_closed());
            if j.waiters.is_empty() && j.inflight == 0 {
                orphaned.push(*k);
            }
        }
        for k in orphaned {
            st.jobs.remove(&k);
            st.stats.orphaned += 1;
        }

        // Deadlines and hedges.
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut no_peers = Vec::new();
        let mut hedge = Vec::new();
        for (k, j) in &st.jobs {
            // Waiting for peer capacity is not failure: the deadline runs from the first send,
            // or from creation only while there are no peers at all (shorter for interactive
            // jobs, so a caller hears "no peers" instead of waiting out the full deadline).
            let overdue = match j.first_sent {
                Some(t) => now.duration_since(t) > self.cfg.deadline,
                None if st.peers.is_empty() => {
                    let wait = if j.low { self.cfg.deadline } else { self.cfg.no_peer_wait };
                    if now.duration_since(j.created) > wait {
                        no_peers.push(*k);
                    }
                    false
                }
                None => false,
            };
            if overdue {
                expired.push(*k);
            } else if j.inflight == 1
                && !j.low
                && !j.hedged
                && !j.queued
                && j.last_sent.is_some_and(|t| now.duration_since(t) > self.hedge_threshold(&st, j.last_peer))
                && (st.stats.hedges as f64) < self.cfg.hedge_budget * (st.stats.requests as f64) + 1.0
            {
                hedge.push(*k);
            }
        }
        for k in expired {
            Self::finish(&mut st, &k, Err(FetchError::Deadline));
        }
        for k in no_peers {
            Self::finish(&mut st, &k, Err(FetchError::NoPeers));
        }
        for k in hedge {
            let j = st.jobs.get_mut(&k).unwrap();
            j.hedged = true;
            j.queued = true;
            st.queue.push_front(k);
            st.stats.hedges += 1;
        }

        // Place queued jobs: simulations first, then background work in what's left.
        self.place(&mut st, false);
        self.place(&mut st, true);
    }

    /// How long a request to `peer` may go unanswered before it's hedged.
    fn hedge_threshold(&self, st: &State, peer: Option<PeerId>) -> Duration {
        let cap = self.cfg.hedge_after;
        if !self.hedge_adaptive.load(std::sync::atomic::Ordering::Relaxed) {
            return cap;
        }
        let Some((mean, dev)) = peer.and_then(|p| st.peers.get(&p)).and_then(|p| Some((p.ewma_ms?, p.dev_ms.unwrap_or(0.0)))) else {
            return cap; // no history for this peer yet
        };
        Duration::from_secs_f64((mean + self.cfg.hedge_k * dev) / 1e3).clamp(self.cfg.hedge_floor, cap)
    }

    fn place(self: &Arc<Self>, st: &mut State, low: bool) {
        let mut deferred = VecDeque::new();
        loop {
            let next = if low { st.queue_lo.pop_front() } else { st.queue.pop_front() };
            let Some(key) = next else { break };
            let Some(job) = st.jobs.get(&key) else { continue };
            if job.low != low || !job.queued {
                // Promoted or demoted while queued (it sits in the other queue). `queued` is
                // checked too so a stray duplicate entry could never send a job twice.
                continue;
            }
            let Some(peer) = self.pick_peer(st, &key, job) else {
                deferred.push_back(key);
                continue;
            };
            let mut keys = vec![key];
            if matches!(key, Key::Code(_)) {
                // Batch further queued code hashes onto the same request.
                let q = if low { &mut st.queue_lo } else { &mut st.queue };
                let mut rest = VecDeque::new();
                while let Some(k) = q.pop_front() {
                    if matches!(k, Key::Code(_)) && keys.len() < self.cfg.code_batch && st.jobs.get(&k).is_some_and(|j| j.low == low && j.queued) {
                        keys.push(k);
                    } else {
                        rest.push_back(k);
                    }
                }
                if low { st.queue_lo = rest } else { st.queue = rest }
                st.stats.code_batches += 1;
            }
            for k in &keys {
                let j = st.jobs.get_mut(k).unwrap();
                j.queued = false;
                j.inflight += 1;
                j.attempts += 1;
                j.tried.insert(peer);
                j.last_peer = Some(peer);
                j.first_sent.get_or_insert_with(Instant::now);
                j.last_sent = Some(Instant::now());
            }
            st.peers.get_mut(&peer).unwrap().inflight += 1;
            st.stats.requests += 1;
            st.stats.requests_low += low as u64;
            let this = self.clone();
            tokio::spawn(async move { this.run(peer, keys).await });
        }
        if low { st.queue_lo = deferred } else { st.queue = deferred }
    }

    async fn run(self: Arc<Self>, peer: PeerId, keys: Vec<Key>) {
        let t = &self.transport;
        let start = Instant::now();
        enum Outcome {
            Values(Vec<(Key, Value)>),
            Unavailable,
            Err(TransportError),
        }
        let outcome = match keys[0] {
            Key::Account { root, address } => match t.account(peer, root, address).await {
                Ok(AccountAnswer::Found(a)) => Outcome::Values(vec![(keys[0], Value::Account(a))]),
                Ok(AccountAnswer::Unavailable) => Outcome::Unavailable,
                Err(e) => Outcome::Err(e),
            },
            Key::Storage { root, address, slot } => match t.storage(peer, root, address, slot).await {
                Ok(SlotAnswer::Found(v)) => Outcome::Values(vec![(keys[0], Value::Storage(v))]),
                Ok(SlotAnswer::Unavailable) => Outcome::Unavailable,
                Err(e) => Outcome::Err(e),
            },
            Key::Code(_) => {
                let hashes: Vec<B256> =
                    keys.iter().map(|k| if let Key::Code(h) = k { *h } else { unreachable!() }).collect();
                match t.codes(peer, hashes).await {
                    Ok(codes) => Outcome::Values(
                        codes.into_iter().map(|c| (Key::Code(keccak256(&c)), Value::Code(c))).collect(),
                    ),
                    Err(e) => Outcome::Err(e),
                }
            }
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1e3;

        let mut st = self.state.lock();
        for k in &keys {
            if let Some(j) = st.jobs.get_mut(k) {
                j.inflight = j.inflight.saturating_sub(1);
            }
        }
        let gone = !st.peers.contains_key(&peer);
        if let Some(p) = st.peers.get_mut(&peer) {
            p.inflight = p.inflight.saturating_sub(1);
        }

        match outcome {
            // An answer with none of the keys asked for (a `GetByteCodes` reply without the
            // codes) is fast because it's empty: scoring it as a success made peers that lack
            // code look like the best. (Review.)
            Outcome::Values(values) if !values.iter().any(|(k, _)| keys.contains(k)) => {
                st.stats.empty_answers += 1;
                for k in &keys {
                    Self::retry_or_fail(&self.cfg, &mut st, *k);
                }
            }
            Outcome::Values(values) => {
                st.stats.ok += 1;
                if let Some(p) = st.peers.get_mut(&peer) {
                    p.on_answer(&self.cfg, elapsed_ms);
                    p.ok += 1;
                    let mean = p.ewma_ms.unwrap_or(elapsed_ms);
                    p.dev_ms = Some(p.dev_ms.map_or(0.0, |d| 0.8 * d + 0.2 * (elapsed_ms - mean).abs()));
                    p.ewma_ms = Some(p.ewma_ms.map_or(elapsed_ms, |e| 0.8 * e + 0.2 * elapsed_ms));
                }
                let mut answered = HashSet::new();
                for (k, v) in values {
                    if keys.contains(&k) {
                        answered.insert(k);
                        if st.jobs.contains_key(&k) {
                            Self::finish(&mut st, &k, Ok(v));
                        } else {
                            st.stats.late_answers_ignored += 1;
                        }
                    }
                }
                // Code hashes this peer didn't have go back to the queue.
                for k in keys.iter().filter(|k| !answered.contains(k)) {
                    Self::retry_or_fail(&self.cfg, &mut st, *k);
                }
            }
            Outcome::Unavailable => {
                st.stats.root_unavailable += 1;
                if let Some(j) = st.jobs.get_mut(&keys[0]) {
                    j.unavailable += 1;
                }
                if let (Some(p), Some(root)) = (st.peers.get_mut(&peer), keys[0].root()) {
                    if p.unavailable_roots.len() > 256 {
                        p.unavailable_roots.retain(|_, t| t.elapsed() < Duration::from_secs(60));
                    }
                    p.unavailable_roots.insert(root, Instant::now());
                }
                Self::retry_or_fail(&self.cfg, &mut st, keys[0]);
            }
            Outcome::Err(e) => {
                if gone || matches!(e, TransportError::Disconnected) {
                    st.stats.disconnects += 1;
                } else {
                    st.stats.errors += 1;
                }
                if let Some(p) = st.peers.get_mut(&peer) {
                    p.on_error(&self.cfg);
                    p.errors += 1;
                    // Penalize slow/failing peers so placement drifts away from them.
                    p.ewma_ms = Some(p.ewma_ms.unwrap_or(300.0) * 1.5);
                }
                for k in &keys {
                    Self::retry_or_fail(&self.cfg, &mut st, *k);
                }
            }
        }
        drop(st);
        self.wake.notify_one();
    }

    fn retry_or_fail(cfg: &Config, st: &mut State, key: Key) {
        let Some(job) = st.jobs.get(&key) else { return };
        if job.inflight > 0 {
            return; // a hedge is still out
        }
        if job.attempts >= cfg.max_attempts {
            let n = job.attempts;
            Self::finish(st, &key, Err(FetchError::Exhausted(n)));
        } else {
            Self::requeue(st, key);
        }
    }
}

#[cfg(test)]
mod tests;
