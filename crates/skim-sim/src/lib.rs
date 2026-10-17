//! Simulation over skim's MVCC snapshots (PROJECT.md §7).
//!
//! [`Simulator::call`] runs one input at a pinned block:
//! 1. **predict** the state it reads ([`predictor`]: learned sets + protocol-aware sources)
//! 2. **fetch** the unknown part in one wave (parent root + BAL replay, [`prefetch::resolve`])
//! 3. **execute** on the snapshot (revm, [`db::SnapDb`])
//! 4. **on a miss** (in `Resolve` mode): expand, fetch, re-run; record the access set
//!
//! In [`Mode::WarmOnly`] it never touches the network: missing state is an error, not a zero.

pub mod db;
pub mod env;
pub mod learned;
pub mod multi;
pub mod predictor;
pub mod prefetch;
pub mod subscription;

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256};
use parking_lot::{Mutex, RwLock};
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::{TxEnv, result::ExecutionResult},
    database::WrapDatabaseRef,
    primitives::TxKind,
};
use skim_bal::StateKey;
use skim_snap::{Scheduler, el::ElTransport};
use skim_state::{BlockRef, Retention, Snapshot, StateError, StateValue, Store};
use skim_snap::Priority;

pub use db::Access;
pub use prefetch::Resolve;

/// Most keys fetched in one predicted wave before running a call.
pub const MAX_PREDICTED_WAVE: usize = 192;

/// The part of a call the predictor keys on.
#[derive(Debug, Clone)]
pub struct CallInput {
    pub to: Address,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Never fetch; missing state is [`SimError::StateUnavailable`].
    WarmOnly,
    /// Predict, fetch and re-run up to `max_rounds` miss rounds.
    Resolve { max_rounds: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("state unavailable locally ({0} keys) in warm-only mode")]
    StateUnavailable(usize),
    #[error("state still incomplete after {0} fetch rounds")]
    Incomplete(usize),
    /// State is served only by SNAP peers (gossip carries writes, not untouched state).
    #[error("no SNAP state peers connected after waiting {0} s; skim is still finding execution-layer peers, retry shortly")]
    NoStatePeers(u64),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("no header for block {0}")]
    NoHeader(u64),
    #[error("state not archived at block {block}: {keys} key(s) uncovered (archived history starts where each key was first known)")]
    NotArchived { block: u64, keys: usize },
    #[error("evm: {0}")]
    Evm(String),
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    /// A simulated transaction failed validation (`eth_simulateV1` error codes).
    #[error("{message}")]
    Invalid { code: i64, message: String },
}

#[derive(Debug)]
pub struct CallOutcome {
    pub result: ExecutionResult,
    pub access: Access,
    /// The predictor's pre-execution fetch, if any keys were unknown.
    pub predicted: Option<Resolve>,
    /// Miss rounds after execution.
    pub rounds: Vec<Resolve>,
    /// Total local execution time over all runs.
    pub exec_us: f64,
    /// Keys the call had to fetch from the network (misses of every run).
    pub fetched_accounts: Vec<Address>,
    pub fetched_storage: Vec<(Address, U256)>,
}

pub struct Simulator {
    pub store: Arc<Store>,
    pub sched: Scheduler<ElTransport>,
    pub predictor: Mutex<predictor::Predictor>,
    /// Headers by number, with their hash computed once on insert.
    headers: RwLock<BTreeMap<u64, (B256, Arc<Header>)>>,
    code_cache: RwLock<std::collections::HashMap<B256, revm::bytecode::Bytecode>>,
    /// Resolve misses at the parent root (peers may not have executed the head yet).
    pub miss_at_parent: bool,
    /// Retention for state fetched on demand (unsubscribed): `Ttl(n)` keeps it current for n
    /// blocks, then evicts it. `Subscribed` keeps everything ever read (pre-M3 behaviour).
    pub on_demand: RwLock<Retention>,
    /// How long an interactive request waits for a SNAP peer when none is connected, before
    /// giving up (`--state-wait`).
    pub state_wait: RwLock<std::time::Duration>,
    /// Miss rounds an interactive call may take (`on_demand.max_fetch_rounds`): each round
    /// fetches the state the previous run stopped at, so deep call chains need more.
    pub max_rounds: std::sync::atomic::AtomicUsize,
}

impl Simulator {
    pub fn new(store: Arc<Store>, sched: Scheduler<ElTransport>, predictor: predictor::Predictor) -> Arc<Self> {
        Arc::new(Self {
            store,
            sched,
            predictor: Mutex::new(predictor),
            headers: RwLock::default(),
            code_cache: RwLock::default(),
            miss_at_parent: true,
            on_demand: RwLock::new(Retention::Ttl(4)),
            max_rounds: std::sync::atomic::AtomicUsize::new(8),
            state_wait: RwLock::new(std::time::Duration::from_secs(60)),
        })
    }

    /// Keeps the on-demand keys a request used current for another TTL (no effect on
    /// subscribed keys). Every read path calls this, so state that is read every block never
    /// expires while it is in use.
    pub fn keep_warm(&self, access: &db::Access) {
        self.keep_warm_with(access, *self.on_demand.read());
    }

    fn keep_warm_with(&self, access: &db::Access, retention: Retention) {
        let Retention::Ttl(n) = retention else { return };
        let mut keys: Vec<StateKey> = access.storage.iter().map(|(a, s)| StateKey::Storage(*a, *s)).collect();
        for a in &access.accounts {
            keys.extend(account_keys(*a));
        }
        self.store.touch_ttl(&keys, n);
    }

    /// Before an interactive fetch wave: if no SNAP peer is connected (e.g. right after a cold
    /// start), waits for one instead of failing, up to `state_wait`.
    pub async fn await_state_peers(&self) -> Result<(), SimError> {
        use skim_snap::Transport as _;
        let wait = *self.state_wait.read();
        let until = std::time::Instant::now() + wait;
        while self.sched.transport().snap_peers().is_empty() {
            if std::time::Instant::now() >= until {
                return Err(SimError::NoStatePeers(wait.as_secs()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok(())
    }

    /// Keeps the header of an applied block (execution env, BLOCKHASH). Pruned with the store
    /// window.
    pub fn insert_header(&self, h: Header) {
        let hash = h.hash_slow();
        let mut hs = self.headers.write();
        hs.insert(h.number, (hash, Arc::new(h)));
        if let Some(head) = hs.keys().next_back().copied() {
            while hs.keys().next().is_some_and(|n| *n + 512 < head) {
                hs.pop_first();
            }
        }
    }

    pub fn header(&self, b: &BlockRef) -> Option<Header> {
        self.header_arc(b).map(|h| (*h).clone())
    }

    /// The header of `b`, shared (no copy, no re-hash on the hot path).
    pub fn header_arc(&self, b: &BlockRef) -> Option<Arc<Header>> {
        if let Some((_, h)) = self.headers.read().get(&b.number).filter(|(hash, _)| *hash == b.hash) {
            return Some(h.clone());
        }
        let (_, rlp) = self.store.archived_block(b.number)?;
        let h = <Header as alloy_rlp::Decodable>::decode(&mut rlp.as_slice()).ok()?;
        (h.hash_slow() == b.hash).then(|| Arc::new(h))
    }

    fn fetch_at(&self, b: &BlockRef) -> BlockRef {
        if self.miss_at_parent { self.store.block(b.number.saturating_sub(1)).unwrap_or(*b) } else { *b }
    }

    /// One local execution (CPU only).
    pub fn execute(&self, snap: &Snapshot, header: &Header, tx: TxEnv) -> Result<(ExecutionResult, Access), SimError> {
        let (store, at) = (self.store.clone(), *snap.block());
        let block_hash = move |n: u64| store.block_hash_at(&at, n);
        let db = db::SnapDb { snap, block_hash: &block_hash, access: Default::default(), code_cache: &self.code_cache };
        let res = {
            let mut evm = Context::mainnet()
                .with_db(WrapDatabaseRef(&db))
                .with_block(env::block_env(header))
                .with_cfg(env::call_cfg())
                .build_mainnet();
            evm.transact(tx).map(|r| r.result).map_err(|e| SimError::Evm(format!("{e:?}")))
        };
        let mut access = db.access.into_inner().unwrap();
        access.dedup();
        res.map(|r| (r, access))
    }

    /// Runs `tx` on the post-state of `b`; fetched state gets the on-demand retention.
    pub async fn call(&self, b: &BlockRef, tx: TxEnv, mode: Mode) -> Result<CallOutcome, SimError> {
        let retention = *self.on_demand.read();
        self.call_retained(b, tx, mode, retention).await
    }

    /// [`Self::call`] committing fetched state with `retention` (subscriptions use `Subscribed`).
    pub async fn call_retained(&self, b: &BlockRef, tx: TxEnv, mode: Mode, retention: Retention) -> Result<CallOutcome, SimError> {
        let snap = self.store.snapshot(b)?;
        let header = self.header_arc(b).ok_or(SimError::NoHeader(b.number))?;
        let at = self.fetch_at(b);
        // Historical (archived) blocks: old roots aren't served by SNAP peers, so never fetch.
        let mode = if snap.is_archived() { Mode::WarmOnly } else { mode };
        let input = match tx.kind {
            TxKind::Call(to) => Some(CallInput { to, data: tx.data.clone() }),
            TxKind::Create => None,
        };
        // Fast path first: most calls find every key local. Prediction runs only after a run
        // misses, and its keys go out in the same wave as the actual misses.
        let mut predicted = None;
        let mut rounds = Vec::new();
        let mut exec_us = 0.0;
        let (mut fetched_accounts, mut fetched_storage) = (Vec::new(), Vec::new());
        loop {
            let t = Instant::now();
            let (result, access) = self.execute(&snap, &header, tx.clone())?;
            exec_us += t.elapsed().as_secs_f64() * 1e6;
            fetched_accounts.extend_from_slice(&access.miss_accounts);
            fetched_storage.extend_from_slice(&access.miss_storage);
            if !access.has_misses() {
                // Learn only from runs that had to fetch: a fully local run teaches nothing new
                // and recording it on every call costs allocations on the hot path.
                if let Some(input) = &input
                    && (predicted.is_some() || !rounds.is_empty())
                {
                    self.predictor.lock().record(input, &access.accounts, &access.storage);
                }
                self.keep_warm_with(&access, retention);
                return Ok(CallOutcome { result, access, predicted, rounds, exec_us, fetched_accounts, fetched_storage });
            }
            if let (Mode::Resolve { .. }, Some(input), None) = (mode, &input, &predicted) {
                let (keys, _) = self.predictor.lock().predict(input, &snap);
                let (mut accts, mut slots) = unknown_split(&snap, &keys);
                // One predicted wave is capped (keys come nearest-first); the rest, if the call
                // needs them, arrive as ordinary miss rounds. An uncapped wave of hundreds of
                // keys on a few peers can take longer than the call is worth (seen: 75 s).
                slots.truncate(MAX_PREDICTED_WAVE.saturating_sub(accts.len()));
                for a in &access.miss_accounts {
                    if !accts.contains(a) {
                        accts.push(*a);
                    }
                }
                for s in &access.miss_storage {
                    if !slots.contains(s) {
                        slots.push(*s);
                    }
                }
                if let Some(&lowest) = access.miss_block_hashes.first() {
                    self.resolve_block_hashes(lowest).await;
                }
                self.await_state_peers().await?;
                predicted = Some(
                    prefetch::resolve_retained(&self.store, &self.sched, &at, &accts, &slots, &access.miss_code, Priority::High, retention)
                        .await,
                );
                continue;
            }
            let max = match mode {
                Mode::WarmOnly if snap.is_archived() => {
                    return Err(SimError::NotArchived {
                        block: b.number,
                        keys: access.miss_accounts.len()
                            + access.miss_storage.len()
                            + access.miss_code.len()
                            + access.miss_block_hashes.len(),
                    });
                }
                Mode::WarmOnly => {
                    return Err(SimError::StateUnavailable(
                        access.miss_accounts.len()
                            + access.miss_storage.len()
                            + access.miss_code.len()
                            + access.miss_block_hashes.len(),
                    ));
                }
                Mode::Resolve { max_rounds } => max_rounds,
            };
            if rounds.len() >= max {
                return Err(SimError::Incomplete(rounds.len()));
            }
            if let Some(&lowest) = access.miss_block_hashes.first() {
                self.resolve_block_hashes(lowest).await;
            }
            let mut slots = access.miss_storage.clone();
            let extra = self.predictor.lock().expand(&snap, &slots);
            slots.extend(unknown_split(&snap, &extra).1);
            slots.sort();
            slots.dedup();
            self.await_state_peers().await?;
            rounds.push(
                prefetch::resolve_retained(
                    &self.store,
                    &self.sched,
                    &at,
                    &access.miss_accounts,
                    &slots,
                    &access.miss_code,
                    Priority::High,
                    retention,
                )
                .await,
            );
        }
    }

    /// Makes canonical hashes down to `lowest` known, walking back by hash from the oldest
    /// known one: each header from a peer is hashed locally and must be the parent of the one
    /// after it. Unverifiable answers are ignored, so a `BLOCKHASH` miss stays a miss.
    pub async fn resolve_block_hashes(&self, lowest: u64) {
        let net = &self.sched.transport().net;
        for _ in 0..4 {
            let Some((oldest, hash)) = self.store.oldest_known_hash() else { return };
            if oldest <= lowest {
                return;
            }
            let want = oldest - lowest + 1;
            let peers: Vec<_> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 69).map(|(id, _)| id).take(4).collect();
            let mut progressed = false;
            for p in peers {
                let (Ok(hs), _) = net.headers_falling(p, hash.into(), want).await else { continue };
                let chain: Vec<(u64, B256, B256)> = hs.iter().map(|h| (h.number, h.hash_slow(), h.parent_hash)).collect();
                if self.store.insert_ancestor_hashes(&chain) > 0 {
                    progressed = true;
                    break;
                }
            }
            if !progressed {
                return;
            }
        }
    }

    /// A single state value at `b`, fetched over SNAP if unknown.
    pub async fn read(&self, b: &BlockRef, key: StateKey) -> Result<U256, SimError> {
        let snap = self.store.snapshot(b)?;
        if let StateValue::Known(v) = snap.get(&key)? {
            // A hit keeps an on-demand key alive like a call's read does: polling a value every
            // block must not let it expire and be fetched again.
            if let Retention::Ttl(n) = *self.on_demand.read() {
                let keys = match key {
                    StateKey::Storage(..) => vec![key],
                    StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) => account_keys(a).to_vec(),
                };
                self.store.touch_ttl(&keys, n);
            }
            return Ok(v);
        }
        if snap.is_archived() {
            return Err(SimError::NotArchived { block: b.number, keys: 1 });
        }
        let at = self.fetch_at(b);
        let ret = *self.on_demand.read();
        self.await_state_peers().await?;
        let r = match key {
            StateKey::Storage(a, s) => {
                prefetch::resolve_retained(&self.store, &self.sched, &at, &[], &[(a, s)], &[], Priority::High, ret).await
            }
            StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) => {
                prefetch::resolve_retained(&self.store, &self.sched, &at, &[a], &[], &[], Priority::High, ret).await
            }
        };
        match snap.get(&key)? {
            StateValue::Known(v) => Ok(v),
            StateValue::Unknown => Err(SimError::Incomplete(r.failed.max(1))),
        }
    }

    /// Account code at `b` (empty for EOAs and absent accounts).
    pub async fn code(&self, b: &BlockRef, a: Address) -> Result<Bytes, SimError> {
        let h = B256::from(self.read(b, StateKey::CodeHash(a)).await?);
        if h == revm::primitives::KECCAK_EMPTY {
            return Ok(Bytes::new());
        }
        if let Some(c) = self.store.code(&h) {
            return Ok(c);
        }
        self.await_state_peers().await?;
        prefetch::resolve(&self.store, &self.sched, &self.fetch_at(b), &[], &[], &[h]).await;
        self.store.code(&h).ok_or(SimError::Incomplete(1))
    }
}

/// An account's three keys (fetched and kept together).
pub fn account_keys(a: Address) -> [StateKey; 3] {
    [StateKey::Balance(a), StateKey::Nonce(a), StateKey::CodeHash(a)]
}

/// Splits keys into the accounts and slots still unknown at `snap`.
pub fn unknown_split(snap: &Snapshot, keys: &[StateKey]) -> (Vec<Address>, Vec<(Address, U256)>) {
    let mut accts = Vec::new();
    let mut slots = Vec::new();
    for k in keys {
        if !matches!(snap.get(k), Ok(StateValue::Unknown)) {
            continue;
        }
        match *k {
            StateKey::Storage(a, s) => slots.push((a, s)),
            StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) => {
                if !accts.contains(&a) {
                    accts.push(a);
                }
            }
        }
    }
    (accts, slots)
}
